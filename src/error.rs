use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io Error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Kube Error: {0}")]
    KubeError(#[from] kube::Error),
    #[error("Finalizer Error: {0}")]
    FinalizerError(#[source] Box<kube::runtime::finalizer::Error<Error>>),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
