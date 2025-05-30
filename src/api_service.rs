use crate::app_context::AppContext;
use crate::indexes::hnsw::types::{HNSWHyperParams, QuantizedDenseVectorEmbedding};
use crate::indexes::hnsw::{DenseInputEmbedding, HNSWIndex};
use crate::indexes::inverted::InvertedIndex;
use crate::indexes::tf_idf::TFIDFIndex;
use crate::indexes::IndexOps;
use crate::metadata::query_filtering::filter_encoded_dimensions;
use crate::metadata::{self, pseudo_level_probs};
use crate::models::buffered_io::BufferManagerFactory;
use crate::models::cache_loader::HNSWIndexCache;
use crate::models::collection::Collection;
use crate::models::collection_transaction::CollectionTransaction;
use crate::models::common::*;
use crate::models::meta_persist::{store_values_range, update_current_version};
use crate::models::prob_node::ProbNode;
use crate::models::types::*;
use crate::models::versioning::Hash;
use crate::quantization::{Quantization, StorageType};
use crate::vector_store::*;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// creates a dense index for a collection
#[allow(clippy::too_many_arguments)]
pub async fn init_hnsw_index_for_collection(
    ctx: Arc<AppContext>,
    collection: Arc<Collection>,
    values_range: Option<(f32, f32)>,
    hnsw_params: HNSWHyperParams,
    quantization_metric: QuantizationMetric,
    distance_metric: DistanceMetric,
    storage_type: StorageType,
    sample_threshold: usize,
    is_configured: bool,
) -> Result<Arc<HNSWIndex>, WaCustomError> {
    let collection_name = &collection.meta.name;
    let collection_path: Arc<Path> = collection.get_path();
    let index_path = collection_path.join("dense_hnsw");
    // ensuring that the index has a separate directory created inside the collection directory
    fs::create_dir_all(&index_path).map_err(|e| WaCustomError::FsError(e.to_string()))?;

    let env = ctx.ain_env.persist.clone();

    let lmdb = MetaDb::from_env(env.clone(), collection_name)
        .map_err(|e| WaCustomError::DatabaseError(e.to_string()))?;

    // Note that setting .write(true).append(true) has the same effect
    // as setting only .append(true)
    //
    // what is the prop file exactly?
    // a file that stores the quantized version of raw vec
    let prop_file = RwLock::new(
        fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(index_path.join("prop.data"))
            .map_err(|e| WaCustomError::FsError(e.to_string()))?,
    );

    let index_manager = Arc::new(BufferManagerFactory::new(
        index_path.clone().into(),
        |root, ver: &Hash| root.join(format!("{}.index", **ver)),
        ProbNode::get_serialized_size(hnsw_params.neighbors_count) * 1000,
    ));

    let level_0_index_manager = Arc::new(BufferManagerFactory::new(
        index_path.clone().into(),
        |root, ver: &Hash| root.join(format!("{}_0.index", **ver)),
        ProbNode::get_serialized_size(hnsw_params.level_0_neighbors_count) * 1000,
    ));
    let vec_raw_manager = BufferManagerFactory::new(
        index_path.into(),
        |root, ver: &Hash| root.join(format!("{}.vec_raw", **ver)),
        8192,
    );
    let distance_metric = Arc::new(RwLock::new(distance_metric));

    // TODO: May be the value can be taken from config
    let cache = HNSWIndexCache::new(
        index_manager.clone(),
        level_0_index_manager.clone(),
        prop_file,
        distance_metric.clone(),
    );
    if let Some(values_range) = values_range {
        store_values_range(&lmdb, values_range).map_err(|e| {
            WaCustomError::DatabaseError(format!("Failed to store values range to LMDB: {}", e))
        })?;
    }
    let values_range = values_range.unwrap_or((-1.0, 1.0));

    let root = create_root_node(
        &quantization_metric,
        storage_type,
        collection.meta.dense_vector.dimension,
        &cache.prop_file,
        *collection.current_version.read().unwrap(),
        &index_manager,
        &level_0_index_manager,
        values_range,
        &hnsw_params,
        *distance_metric.read().unwrap(),
        collection.meta.metadata_schema.as_ref(),
    )?;

    index_manager.flush_all()?;
    // ---------------------------
    // -- TODO level entry ratio
    // ---------------------------
    let factor_levels = 4.0;

    // If metadata schema is supported, the level_probs needs to be
    // adjusted to accommodate only pseudo nodes in the higher layers
    let lp = match &collection.meta.metadata_schema {
        Some(metadata_schema) => {
            // @TODO(vineet): Unnecessary computation of
            // pseudo_weighted_dimensions. Just the no. of pseudo
            // replicas should be sufficient.
            let replica_dims = metadata_schema.pseudo_weighted_dimensions(1);
            let plp = pseudo_level_probs(hnsw_params.num_layers, replica_dims.len() as u16);
            // @TODO(vineet): Super hacky
            let num_lower_layers = plp.iter().filter(|(p, _)| *p == 0.0).count() - 1;
            let num_higher_layers = hnsw_params.num_layers - (num_lower_layers as u8);
            let mut lp = vec![];
            for i in 0..num_higher_layers {
                // no actual replica nodes in higher layers
                lp.push((1.0, hnsw_params.num_layers - i))
            }
            let mut lower_lp = generate_level_probs(factor_levels, num_lower_layers as u8);
            lp.append(&mut lower_lp);
            lp
        }
        None => generate_level_probs(factor_levels, hnsw_params.num_layers),
    };

    let hnsw_index = Arc::new(HNSWIndex::new(
        root,
        lp,
        collection.meta.dense_vector.dimension,
        quantization_metric,
        distance_metric,
        storage_type,
        hnsw_params,
        cache,
        vec_raw_manager,
        values_range,
        sample_threshold,
        is_configured,
    ));

    ctx.ain_env
        .collections_map
        .insert_hnsw_index(&collection, hnsw_index.clone())?;

    // If the collection has metadata schema, we create pseudo replica
    // nodes to ensure that the query vectors with metadata dimensions
    // are reachable from the root node.
    if collection.meta.metadata_schema.is_some() {
        let num_dims = collection.meta.dense_vector.dimension;
        let pseudo_vals: Vec<f32> = vec![1.0; num_dims];
        // The pseudo vector's id will be equal to the max number that
        // can be represented with 56 bits. This is because of how we
        // are calculating the combined id for nodes having metadata
        // dims. See `ProbNode.get_id` implementation. Perhaps it'd be
        // a good idea to derive this value from root.
        let pseudo_vec_id = VectorId(u64::pow(2, 56) - 1);
        let pseudo_vec = DenseInputEmbedding(pseudo_vec_id, pseudo_vals, None, true);
        let transaction = CollectionTransaction::new(collection.clone())?;
        hnsw_index.run_upload(&collection, vec![pseudo_vec], &transaction, &ctx.config)?;
        let (id, version_number) = (transaction.id, transaction.version_number);
        transaction.pre_commit(&collection, &ctx.config)?;
        *collection.current_version.write().unwrap() = id;
        collection
            .vcs
            .set_branch_version("main", version_number.into(), id)?;
        update_current_version(&collection.lmdb, id)?;
    }

    Ok(hnsw_index)
}

/// creates an inverted index for a collection
pub async fn init_inverted_index_for_collection(
    ctx: Arc<AppContext>,
    collection: &Collection,
    quantization_bits: u8,
    sample_threshold: usize,
) -> Result<Arc<InvertedIndex>, WaCustomError> {
    let collection_path: Arc<Path> = collection.get_path();
    let index_path = collection_path.join("sparse_inverted_index");
    fs::create_dir_all(&index_path).map_err(|e| WaCustomError::FsError(e.to_string()))?;

    // what is the difference between vec_raw_manager and index_manager?
    // vec_raw_manager manages persisting raw embeddings/vectors on disk
    // index_manager manages persisting index data on disk
    let vec_raw_manager = BufferManagerFactory::new(
        index_path.clone().into(),
        |root, ver: &u8| root.join(format!("{}.vec_raw", ver)),
        8192,
    );

    let index = Arc::new(InvertedIndex::new(
        index_path.clone(),
        vec_raw_manager,
        quantization_bits,
        sample_threshold,
        ctx.config.inverted_index_data_file_parts,
    )?);

    ctx.ain_env
        .collections_map
        .insert_inverted_index(&collection, index.clone())?;
    Ok(index)
}

/// creates an inverted index for a collection
pub async fn init_tf_idf_index_for_collection(
    ctx: Arc<AppContext>,
    collection: &Collection,
    sample_threshold: usize,
    store_raw_text: bool,
    k1: f32,
    b: f32,
) -> Result<Arc<TFIDFIndex>, WaCustomError> {
    let collection_path: Arc<Path> = collection.get_path();
    let index_path = collection_path.join("tf_idf_index");
    fs::create_dir_all(&index_path).map_err(|e| WaCustomError::FsError(e.to_string()))?;

    let vec_raw_manager = BufferManagerFactory::new(
        index_path.clone().into(),
        |root, ver: &u8| root.join(format!("{}.vec_raw", ver)),
        8192,
    );

    let index = Arc::new(TFIDFIndex::new(
        index_path.clone(),
        vec_raw_manager,
        ctx.config.inverted_index_data_file_parts,
        sample_threshold,
        store_raw_text,
        k1,
        b,
    )?);

    ctx.ain_env
        .collections_map
        .insert_tf_idf_index(&collection, index.clone())?;
    Ok(index)
}

pub async fn ann_vector_query(
    ctx: Arc<AppContext>,
    collection: &Collection,
    hnsw_index: Arc<HNSWIndex>,
    query: Vec<f32>,
    metadata_filter: Option<metadata::Filter>,
    k: Option<usize>,
) -> Result<Vec<(VectorId, MetricResult)>, WaCustomError> {
    let vec_hash = VectorId(u64::MAX - 1);
    let vector_list = hnsw_index.quantization_metric.read().unwrap().quantize(
        &query,
        *hnsw_index.storage_type.read().unwrap(),
        *hnsw_index.values_range.read().unwrap(),
    )?;

    let vec_emb = QuantizedDenseVectorEmbedding {
        quantized_vec: Arc::new(vector_list.clone()),
        hash_vec: vec_hash.clone(),
    };

    let hnsw_params_guard = hnsw_index.hnsw_params.read().unwrap();

    let query_filter_dims = metadata_filter.map(|filter| {
        let metadata_schema = collection.meta.metadata_schema.as_ref().unwrap();
        filter_encoded_dimensions(metadata_schema, &filter).unwrap()
    });

    let results = ann_search(
        &ctx.config,
        hnsw_index.clone(),
        vec_emb,
        query_filter_dims.as_ref(),
        hnsw_index.get_root_vec(),
        HNSWLevel(hnsw_params_guard.num_layers),
        &hnsw_params_guard,
    )?;
    drop(hnsw_params_guard);
    let output = finalize_ann_results(collection, &hnsw_index, results, &query, k)?;
    Ok(output)
}

pub async fn batch_ann_vector_query(
    ctx: Arc<AppContext>,
    collection: &Collection,
    hnsw_index: Arc<HNSWIndex>,
    queries: Vec<Vec<f32>>,
    metadata_filter: Option<metadata::Filter>,
    k: Option<usize>,
) -> Result<Vec<Vec<(VectorId, MetricResult)>>, WaCustomError> {
    let query_filter_dims = metadata_filter.map(|filter| {
        let metadata_schema = collection.meta.metadata_schema.as_ref().unwrap();
        filter_encoded_dimensions(metadata_schema, &filter).unwrap()
    });

    queries
        .into_par_iter()
        .map(|query| {
            let vec_hash = VectorId(u64::MAX - 1);
            let vector_list = hnsw_index.quantization_metric.read().unwrap().quantize(
                &query,
                *hnsw_index.storage_type.read().unwrap(),
                *hnsw_index.values_range.read().unwrap(),
            )?;

            let vec_emb = QuantizedDenseVectorEmbedding {
                quantized_vec: Arc::new(vector_list.clone()),
                hash_vec: vec_hash.clone(),
            };

            let hnsw_params = hnsw_index.hnsw_params.read().unwrap();
            let results = ann_search(
                &ctx.config,
                hnsw_index.clone(),
                vec_emb,
                query_filter_dims.as_ref(),
                hnsw_index.get_root_vec(),
                HNSWLevel(hnsw_params.num_layers),
                &hnsw_params,
            )?;
            let output = finalize_ann_results(collection, &hnsw_index, results, &query, k)?;
            Ok::<_, WaCustomError>(output)
        })
        .collect()
}
