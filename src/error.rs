/// This is the enumeration of all the different kinds of errors which this
/// crate generates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The raw request to be sent to the server could not be generated.
    #[error("unable to generate HTTP request")]
    BadRequest(#[source] rhymuweb::Error),

    /// The raw response received from the server could not be parsed.
    #[error("unable to parse HTTP response")]
    BadResponse(#[source] rhymuweb::Error),

    /// The connection to the server was lost while awaiting the response
    /// to a request.
    #[error("disconnected from server")]
    Disconnected,

    /// The attached host in the target URI did not parse as valid text.
    #[error("unable to parse target URI host as text")]
    HostNotValidText(Vec<u8>),

    /// No authority was provided in the target URI, so the user agent doesn't
    /// know to whom to send the request.
    #[error("no authority provided in target URI")]
    NoTargetAuthority(rhymuri::Uri),

    /// An error occurred during the initial TLS handshake with the server.
    #[error("error performing TLS handshake with server")]
    TlsHandshake(#[source] std::io::Error),

    /// The user agent was unable to establish a connection to the
    /// given server host/port asthe target of the request.
    #[error("unable to connect to server host/port")]
    UnableToConnect(#[source] std::io::Error),

    /// The user agent could not determine at what port to contact the
    /// server for the target identified by the attached URI.
    #[error("unable to determine target server port from URI")]
    UnableToDetermineServerPort(rhymuri::Uri),

    /// The user agent connected to a server but could not get the
    /// server's address from the connection.
    #[error("unable to get peer address from connection")]
    UnableToGetPeerAddress(#[source] std::io::Error),

    /// The user agent encountered an error attempting to receive the
    /// response from the server.
    #[error("unable to receive response from server")]
    UnableToReceive(#[source] std::io::Error),

    /// The user agent encountered an error attempting to send the
    /// request to the server.
    #[error("unable to send request to server")]
    UnableToSend(#[source] std::io::Error),
}
