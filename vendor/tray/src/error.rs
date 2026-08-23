use thiserror::Error;

/// Errors that can occur when creating or interacting with a tray icon.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    OsError(#[from] std::io::Error),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error(transparent)]
    PngEncodingError(#[from] png::EncodingError),
    #[error("not on the main thread")]
    NotMainThread,
    #[error("not supported: {0}")]
    NotSupported(&'static str),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    X11ConnectError(#[from] x11rb::errors::ConnectError),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    X11ConnectionError(#[from] x11rb::errors::ConnectionError),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    X11ReplyError(#[from] x11rb::errors::ReplyError),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    X11ReplyOrIdError(#[from] x11rb::errors::ReplyOrIdError),
}

/// A specialized [`Result`](std::result::Result) type for tray operations.
pub type Result<T> = std::result::Result<T, Error>;
