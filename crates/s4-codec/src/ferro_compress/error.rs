use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unknown compression algorithm: {0}")]
    UnknownAlgo(String),

    #[error("algorithm {0} is not supported by this backend")]
    UnsupportedAlgo(super::Algo),

    #[error("batch length mismatch: inputs={inputs}, outputs={outputs}")]
    BatchLenMismatch { inputs: usize, outputs: usize },

    #[error("compression failed: {0}")]
    Compress(String),

    #[error("decompression failed: {0}")]
    Decompress(String),

    #[error("backend not available: {0}")]
    BackendUnavailable(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
