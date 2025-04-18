#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{
    fmt::Debug,
    sync::atomic::{AtomicPtr, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::models::{
    buffered_io::BufIoError,
    cache_loader::{HNSWIndexCache, InvertedIndexCache, TFIDFIndexCache},
    inverted_index::InvertedIndexNodeData,
    prob_node::ProbNode,
    tf_idf_index::TFIDFIndexNodeData,
    types::FileOffset,
    versioning::Hash,
};

use super::lazy_item_array::ProbLazyItemArray;

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub offset: FileOffset,
    pub version_number: u16,
    pub version_id: Hash,
}

pub fn largest_power_of_4_below(x: u16) -> u8 {
    // This function is used to calculate the largest power of 4 (4^n) such that
    // 4^n <= x, where x represents the gap between the current version and the
    // target version in our version control system.
    //
    // The system uses an exponentially spaced versioning scheme, where each
    // checkpoint is spaced by powers of 4 (1, 4, 16, 64, etc.). This minimizes
    // the number of intermediate versions stored, allowing efficient lookups
    // and updates by focusing only on meaningful checkpoints.
    //
    // The input x should not be zero because finding a "largest power of 4 below zero"
    // is undefined, as zero does not have any significant bits for such a calculation.
    assert_ne!(x, 0, "x should not be zero");

    // must be small enough to fit inside u8
    let msb_position = (15 - x.leading_zeros()) as u8; // Find the most significant bit's position
    msb_position / 2 // Return the power index of the largest 4^n ≤ x
}

#[derive(PartialEq, Debug)]
pub struct ReadyState<T> {
    pub data: T,
    pub file_offset: FileOffset,
    pub version_id: Hash,
    pub version_number: u16,
}

// not cloneable
#[derive(PartialEq, Debug)]
pub enum ProbLazyItemState<T> {
    Ready(ReadyState<T>),
    Pending(FileIndex),
}

impl<T> ProbLazyItemState<T> {
    pub fn get_version_number(&self) -> u16 {
        match self {
            Self::Pending(file_index) => file_index.version_number,
            Self::Ready(state) => state.version_number,
        }
    }

    pub fn get_version_id(&self) -> Hash {
        match self {
            Self::Pending(file_index) => file_index.version_id,
            Self::Ready(state) => state.version_id,
        }
    }
}

pub struct ProbLazyItem<T> {
    state: AtomicPtr<ProbLazyItemState<T>>,
    pub is_level_0: bool,
}

impl<T: PartialEq> PartialEq for ProbLazyItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.is_level_0 == other.is_level_0
            && unsafe {
                *self.state.load(Ordering::Relaxed) == *other.state.load(Ordering::Relaxed)
            }
    }
}

impl<T: Debug> Debug for ProbLazyItem<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbLazyItem")
            .field("state", unsafe { &*self.state.load(Ordering::Relaxed) })
            .field("is_level_0", &self.is_level_0)
            .finish()
    }
}

#[allow(unused)]
impl<T> ProbLazyItem<T> {
    pub fn new(
        data: T,
        version_id: Hash,
        version_number: u16,
        is_level_0: bool,
        file_offset: FileOffset,
    ) -> *mut Self {
        Box::into_raw(Box::new(Self {
            state: AtomicPtr::new(Box::into_raw(Box::new(ProbLazyItemState::Ready(
                ReadyState {
                    data,
                    file_offset,
                    version_id,
                    version_number,
                },
            )))),
            is_level_0,
        }))
    }

    pub fn new_from_state(state: ProbLazyItemState<T>, is_level_0: bool) -> *mut Self {
        Box::into_raw(Box::new(Self {
            state: AtomicPtr::new(Box::into_raw(Box::new(state))),
            is_level_0,
        }))
    }

    pub fn new_pending(file_index: FileIndex, is_level_0: bool) -> *mut Self {
        Box::into_raw(Box::new(Self {
            state: AtomicPtr::new(Box::into_raw(Box::new(ProbLazyItemState::Pending(
                file_index,
            )))),
            is_level_0,
        }))
    }

    pub fn unsafe_get_state(&self) -> &ProbLazyItemState<T> {
        // SAFETY: caller must make sure the state is not dropped by some other thread
        unsafe { &*self.state.load(Ordering::Acquire) }
    }

    pub fn set_state(&self, new_state: ProbLazyItemState<T>) {
        let old_state = self
            .state
            .swap(Box::into_raw(Box::new(new_state)), Ordering::SeqCst);
        unsafe {
            // SAFETY: state must be a valid pointer
            drop(Box::from_raw(old_state));
        }
    }

    pub fn is_ready(&self) -> bool {
        unsafe {
            matches!(
                &*self.state.load(Ordering::Acquire),
                ProbLazyItemState::Ready(_)
            )
        }
    }

    pub fn is_pending(&self) -> bool {
        unsafe {
            matches!(
                &*self.state.load(Ordering::Acquire),
                ProbLazyItemState::Pending(_)
            )
        }
    }

    pub fn get_lazy_data<'a>(&self) -> Option<&'a T> {
        unsafe {
            match &*self.state.load(Ordering::Acquire) {
                ProbLazyItemState::Pending(_) => None,
                ProbLazyItemState::Ready(state) => Some(&state.data),
            }
        }
    }

    pub fn get_file_index(&self) -> FileIndex {
        unsafe {
            match &*self.state.load(Ordering::Acquire) {
                ProbLazyItemState::Pending(file_index) => *file_index,
                ProbLazyItemState::Ready(state) => FileIndex {
                    offset: state.file_offset,
                    version_number: state.version_number,
                    version_id: state.version_id,
                },
            }
        }
    }

    pub fn get_current_version_id(&self) -> Hash {
        unsafe { (*self.state.load(Ordering::Acquire)).get_version_id() }
    }

    pub fn get_current_version_number(&self) -> u16 {
        unsafe { (*self.state.load(Ordering::Acquire)).get_version_number() }
    }
}

impl ProbLazyItem<ProbNode> {
    pub fn try_get_data<'a>(&self, cache: &HNSWIndexCache) -> Result<&'a ProbNode, BufIoError> {
        unsafe {
            match &*self.state.load(Ordering::Relaxed) {
                ProbLazyItemState::Ready(state) => Ok(&state.data),
                ProbLazyItemState::Pending(file_index) => {
                    (*(cache.get_object(*file_index, self.is_level_0)?)).try_get_data(cache)
                }
            }
        }
    }

    pub fn add_version(
        this: *mut Self,
        version: *mut Self,
        cache: &HNSWIndexCache,
    ) -> Result<Result<*mut Self, *mut Self>, BufIoError> {
        let data = unsafe { &*this }.try_get_data(cache)?;
        let versions = &data.versions;

        let (_, latest_local_version_number) =
            Self::get_latest_version_inner(this, versions, cache)?;

        let result =
            Self::add_version_inner(this, version, 0, latest_local_version_number + 1, cache)?;

        Ok(result)
    }

    pub fn add_version_inner(
        this: *mut Self,
        version: *mut Self,
        self_relative_version_number: u16,
        target_relative_version_number: u16,
        cache: &HNSWIndexCache,
    ) -> Result<Result<*mut Self, *mut Self>, BufIoError> {
        let target_diff = target_relative_version_number - self_relative_version_number;
        if target_diff == 0 {
            return Ok(Err(this));
        }
        let index = largest_power_of_4_below(target_diff);
        let data = unsafe { &*this }.try_get_data(cache)?;
        let versions = &data.versions;

        if let Some(existing_version) = versions.get(index as usize) {
            Self::add_version_inner(
                existing_version,
                version,
                self_relative_version_number + (1 << (2 * index)),
                target_relative_version_number,
                cache,
            )
        } else {
            debug_assert_eq!(versions.len(), index as usize);
            versions.push(version);
            Ok(Ok(this))
        }
    }

    pub fn get_latest_version(
        this: *mut Self,
        cache: &HNSWIndexCache,
    ) -> Result<(*mut Self, u16), BufIoError> {
        let data = unsafe { &*this }.try_get_data(cache)?;
        let versions = &data.versions;

        Self::get_latest_version_inner(this, versions, cache)
    }

    fn get_latest_version_inner<const LEN: usize>(
        this: *mut Self,
        versions: &ProbLazyItemArray<ProbNode, LEN>,
        cache: &HNSWIndexCache,
    ) -> Result<(*mut Self, u16), BufIoError> {
        if let Some(last) = versions.last() {
            let (latest_version, relative_local_version_number) =
                Self::get_latest_version(last, cache)?;
            Ok((
                latest_version,
                (1u16 << ((versions.len() as u8 - 1) * 2)) + relative_local_version_number,
            ))
        } else {
            Ok((this, 0))
        }
    }

    pub fn get_root_version(
        this: *mut Self,
        cache: &HNSWIndexCache,
    ) -> Result<*mut Self, BufIoError> {
        let self_ = unsafe { &*this };
        let root = self_.try_get_data(cache)?.root_version;
        Ok(if root.is_null() { this } else { root })
    }

    pub fn get_version(
        this: *mut Self,
        version: u16,
        cache: &HNSWIndexCache,
    ) -> Result<Option<*mut Self>, BufIoError> {
        let self_ = unsafe { &*this };
        let version_number = self_.get_current_version_number();
        let data = self_.try_get_data(cache)?;
        let versions = &data.versions;

        if version < version_number {
            return Ok(None);
        }

        if version == version_number {
            return Ok(Some(this));
        }

        let Some(mut prev) = versions.get(0) else {
            return Ok(None);
        };
        let mut i = 1;
        while let Some(next) = versions.get(i) {
            if version < unsafe { &*next }.get_current_version_number() {
                return Self::get_version(prev, version, cache);
            }
            prev = next;
            i += 1;
        }

        Self::get_version(prev, version, cache)
    }
}

impl ProbLazyItem<InvertedIndexNodeData> {
    pub fn try_get_data<'a>(
        &self,
        cache: &InvertedIndexCache,
        dim: u32,
    ) -> Result<&'a InvertedIndexNodeData, BufIoError> {
        unsafe {
            match &*self.state.load(Ordering::Relaxed) {
                ProbLazyItemState::Ready(state) => Ok(&state.data),
                ProbLazyItemState::Pending(file_index) => {
                    let offset = file_index.offset;
                    (*(cache.get_data(offset, (dim % cache.data_file_parts as u32) as u8)?))
                        .try_get_data(cache, dim)
                }
            }
        }
    }
}

impl ProbLazyItem<TFIDFIndexNodeData> {
    pub fn try_get_data<'a>(
        &self,
        cache: &TFIDFIndexCache,
        dim: u32,
    ) -> Result<&'a TFIDFIndexNodeData, BufIoError> {
        unsafe {
            match &*self.state.load(Ordering::Relaxed) {
                ProbLazyItemState::Ready(state) => Ok(&state.data),
                ProbLazyItemState::Pending(file_index) => {
                    let offset = file_index.offset;
                    (*(cache.get_data(offset, (dim % cache.data_file_parts as u32) as u8)?))
                        .try_get_data(cache, dim)
                }
            }
        }
    }
}

impl<T> Drop for ProbLazyItem<T> {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: state must be a valid pointer
            drop(Box::from_raw(self.state.load(Ordering::SeqCst)));
        }
    }
}
