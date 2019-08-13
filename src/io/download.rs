pub use inner::*;

#[cfg(feature = "reqwest")]
mod inner {
    pub use reqwest::{Client, Error, Response, StatusCode};

    use headers::{HeaderMap, HeaderMapExt, Range};
    use reqwest::{RedirectPolicy, Result};

    pub const MAX_HTTP_ATTEMPTS: usize = 4;
    pub const MAX_HTTP_REDIRECTS_ALLOWED: usize = 10;

    pub fn get(url: &str) -> Result<Response> {
        Client::new().get(url).send()
    }

    pub fn get_range(client: &mut Client, url: &str, start: u64, end: u64) -> Result<Response> {
        let mut headers = HeaderMap::new();
        headers.typed_insert(Range::bytes(start..=end).unwrap());
        client.get(url).headers(headers).send()
    }

    pub fn head(url: &str) -> Result<Response> {
        // First, we actually do a HEAD request on the URL for the data file.
        // If it's redirected, we update our URL to follow the redirects. If
        // we didn't do this separately, the index file would have to be the
        // one with the redirect setup, which would be confusing and annoying.

        let redirect_policy = RedirectPolicy::custom(|attempt| {
            // In the process of resolving the file url it might be neccesary
            // to stop at a certain level of redirection. This might be required
            // because some hosts might redirect to a version of the url where
            // it isn't possible to select the index file by appending .index.gz.
            // (This mostly happens because CDNs redirect to the file hash.)
            if attempt.previous().len() >= MAX_HTTP_REDIRECTS_ALLOWED {
                attempt.too_many_redirects()
            } else if let Some(segments) = attempt.url().clone().path_segments() {
                let follow = segments
                    .last()
                    .map(|file| file.contains('.'))
                    .unwrap_or(true);
                if follow {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            } else {
                attempt.follow()
            }
        });

        Client::builder()
            .redirect(redirect_policy)
            .build()?
            .head(url)
            .send()
    }
}

#[cfg(not(feature = "reqwest"))]
mod inner {
    use std::error;
    use std::fmt::{self, Display, Formatter};

    pub const MAX_HTTP_ATTEMPTS: usize = 1;

    #[derive(Debug)]
    pub struct Error;

    impl Display for Error {
        fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
            write!(fmt, "Tectonic was compiled without download support")
        }
    }

    impl error::Error for Error {}

    pub type Result<T> = std::result::Result<T, Error>;

    pub use http::StatusCode;

    pub struct Client {}

    impl Client {
        pub fn new() -> Client {
            Client {}
        }
    }

    pub struct Response {}

    impl Response {
        pub fn status(&self) -> StatusCode {
            unreachable!()
        }

        pub fn url(&self) -> url::Url {
            unreachable!()
        }
    }

    impl ::std::io::Read for Response {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            unreachable!()
        }
    }

    pub fn get(_: &str) -> Result<Response> {
        Err(Error)
    }

    pub fn get_range(_: &mut Client, _url: &str, _start: u64, _end: u64) -> Result<Response> {
        Err(Error)
    }

    pub fn head(_: &str) -> Result<Response> {
        Err(Error)
    }
}
