//! volmount-core: 核心库，包含后端抽象、btree、journal、缓存

pub mod alloc;
pub mod block_device;
pub mod btree;

pub mod config;
pub mod journal;
pub mod lock;
pub mod meta;
pub mod recovery;
pub mod snap;
pub mod storage;
pub mod subvol;
pub mod types;
pub mod volume;

pub use meta::VolumeMeta;
pub use types::*;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
