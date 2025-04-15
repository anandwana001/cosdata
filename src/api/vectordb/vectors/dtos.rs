use std::fmt;

use crate::metadata::MetadataFields;

use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize,
};

use crate::{indexes::inverted::types::SparsePair, models::types::VectorId};

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct CreateDenseVectorDto {
    pub id: VectorId,
    pub values: Vec<f32>,
    pub metadata: Option<MetadataFields>,
}

#[derive(Serialize, Debug)]
pub(crate) struct CreateSparseVectorDto {
    pub id: VectorId,
    pub values: Vec<SparsePair>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct CreateSparseIdfDocumentDto {
    pub id: VectorId,
    pub text: String,
}

impl<'de> Deserialize<'de> for CreateSparseVectorDto {
    fn deserialize<D>(deserializer: D) -> Result<CreateSparseVectorDto, D::Error>
    where
        D: Deserializer<'de>,
    {
        // A custom visitor to process the deserialization
        struct CreateSparseVectorDtoVisitor;

        impl<'de> Visitor<'de> for CreateSparseVectorDtoVisitor {
            type Value = CreateSparseVectorDto;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "struct CreateSparseVectorDto")
            }

            fn visit_map<M>(self, mut map: M) -> Result<CreateSparseVectorDto, M::Error>
            where
                M: de::MapAccess<'de>,
            {
                let mut id = None;
                let mut values: Option<Vec<f32>> = None;
                let mut indices: Option<Vec<u32>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value()?);
                        }
                        "values" => {
                            if values.is_some() {
                                return Err(de::Error::duplicate_field("values"));
                            }
                            values = Some(map.next_value()?);
                        }
                        "indices" => {
                            if indices.is_some() {
                                return Err(de::Error::duplicate_field("indices"));
                            }
                            indices = Some(map.next_value()?);
                        }
                        _ => {
                            return Err(de::Error::unknown_field(
                                key.as_str(),
                                &["id", "values", "indices"],
                            ));
                        }
                    }
                }

                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
                let values = values.ok_or_else(|| de::Error::missing_field("values"))?;
                let indices = indices.ok_or_else(|| de::Error::missing_field("indices"))?;

                // Combine the values and indices into a Vec<SparsePair>
                let values = indices
                    .into_iter()
                    .zip(values)
                    .map(|(index, value)| SparsePair(index, value))
                    .collect();

                Ok(CreateSparseVectorDto { id, values })
            }
        }

        deserializer.deserialize_map(CreateSparseVectorDtoVisitor)
    }
}

#[derive(Debug)]
pub(crate) enum CreateVectorDto {
    Dense(CreateDenseVectorDto),
    Sparse(CreateSparseVectorDto),
    SparseIdf(CreateSparseIdfDocumentDto),
}

impl<'de> Deserialize<'de> for CreateVectorDto {
    fn deserialize<D>(deserializer: D) -> Result<CreateVectorDto, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            IndexType,
            IsIdf,
            Id,
            Values,
            Indices,
            Text,
            Metadata,
        }

        #[derive(Deserialize)]
        struct RawMap {
            #[serde(rename = "index_type")]
            index_type: String,

            #[serde(default, rename = "isIDF")]
            is_idf: bool,

            #[serde(flatten)]
            rest: serde_json::Value,
        }

        let RawMap {
            index_type,
            is_idf,
            rest,
        } = RawMap::deserialize(deserializer)?;

        match (index_type.as_str(), is_idf) {
            ("dense", _) => {
                let dense = serde_json::from_value(rest).map_err(de::Error::custom)?;
                Ok(CreateVectorDto::Dense(dense))
            }
            ("sparse", true) => {
                let sparse_idf = serde_json::from_value(rest).map_err(de::Error::custom)?;
                Ok(CreateVectorDto::SparseIdf(sparse_idf))
            }
            ("sparse", false) => {
                let sparse = serde_json::from_value(rest).map_err(de::Error::custom)?;
                Ok(CreateVectorDto::Sparse(sparse))
            }
            (other, _) => Err(de::Error::unknown_variant(other, &["dense", "sparse"])),
        }
    }
}

#[derive(Serialize)]
pub(crate) enum CreateVectorResponseDto {
    Dense(CreateDenseVectorDto),
    Sparse(CreateSparseVectorDto),
    SparseIdf(CreateSparseIdfDocumentDto),
}

#[derive(Deserialize)]
pub(crate) struct UpdateVectorDto {
    pub values: Vec<f32>,
}

#[derive(Serialize)]
pub(crate) struct UpdateVectorResponseDto {
    pub id: VectorId,
    pub values: Vec<f32>,
    // pub created_at: String
}

#[derive(Serialize)]
pub(crate) struct SimilarVector {
    pub id: VectorId,
    pub score: f32,
}
