#[cfg(not(target_os = "toyos"))]
mod listener;
#[cfg(not(target_os = "toyos"))]
pub use self::listener::TcpListener;

#[cfg(not(target_os = "toyos"))]
mod stream;
#[cfg(not(target_os = "toyos"))]
pub use self::stream::TcpStream;

#[cfg(target_os = "toyos")]
pub(crate) mod toyos_stream;
#[cfg(target_os = "toyos")]
pub use self::toyos_stream::TcpStream;

#[cfg(target_os = "toyos")]
mod toyos_listener;
#[cfg(target_os = "toyos")]
pub use self::toyos_listener::TcpListener;
