//! > This crate is maintained by the Cargo team for use by the wider
//! > ecosystem. This crate follows semver compatibility for its APIs.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::time::Instant;

use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use serde::{Deserialize, Serialize};
use url::Url;

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// HTTP transport abstraction
// ---------------------------------------------------------------------------

/// Response from an HTTP request.
pub struct HttpResponse {
    pub status: u32,
    pub headers: Vec<String>,
    pub body: Vec<u8>,
}

/// HTTP method for requests.
pub enum HttpMethod {
    Get,
    Put,
    Delete,
}

/// Trait for pluggable HTTP backends (curl, reqwest, ureq, etc.).
///
/// Implementors handle the actual HTTP transport. The `Registry` struct
/// uses this trait to remain backend-agnostic.
pub trait HttpHandle {
    /// Perform an HTTP request.
    ///
    /// `url`: full URL to request
    /// `headers`: list of "Name: Value" header strings
    /// `method`: GET, PUT, or DELETE
    /// `body`: optional request body
    fn request(
        &mut self,
        url: &str,
        headers: &[String],
        method: HttpMethod,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse>;
}

// curl-based HttpHandle implementation
#[cfg(feature = "curl")]
mod curl_handle {
    use curl::easy::{Easy, List};

    use super::*;

    /// Wraps a `curl::easy::Easy` as an `HttpHandle`.
    pub struct CurlHttpHandle(pub Easy);

    impl HttpHandle for CurlHttpHandle {
        fn request(
            &mut self,
            url: &str,
            headers: &[String],
            method: HttpMethod,
            body: Option<&[u8]>,
        ) -> Result<HttpResponse> {
            self.0.url(url)?;

            let mut list = List::new();
            for h in headers {
                list.append(h)?;
            }
            self.0.http_headers(list)?;

            match method {
                HttpMethod::Get => self.0.get(true)?,
                HttpMethod::Put => self.0.put(true)?,
                HttpMethod::Delete => self.0.custom_request("DELETE")?,
            }

            if let Some(body) = body {
                self.0.upload(true)?;
                self.0.in_filesize(body.len() as u64)?;
            }

            let mut resp_headers = Vec::new();
            let mut resp_body = Vec::new();
            let mut body_to_send = body.unwrap_or(&[]);
            {
                let mut transfer = self.0.transfer();
                transfer.read_function(|buf| {
                    let n = std::cmp::min(buf.len(), body_to_send.len());
                    buf[..n].copy_from_slice(&body_to_send[..n]);
                    body_to_send = &body_to_send[n..];
                    Ok(n)
                })?;
                transfer.write_function(|data| {
                    resp_body.extend_from_slice(data);
                    Ok(data.len())
                })?;
                transfer.header_function(|data| {
                    let s = String::from_utf8_lossy(data).trim().to_string();
                    if !s.contains('\n') {
                        resp_headers.push(s);
                    }
                    true
                })?;
                transfer.perform()?;
            }

            let status = self.0.response_code()?;
            Ok(HttpResponse {
                status,
                headers: resp_headers,
                body: resp_body,
            })
        }
    }
}

#[cfg(feature = "curl")]
pub use curl_handle::CurlHttpHandle;

// ---------------------------------------------------------------------------
// Data types (backend-agnostic)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct Crate {
    pub name: String,
    pub description: Option<String>,
    pub max_version: String,
}

/// This struct is serialized as JSON and sent as metadata ahead of the crate
/// tarball when publishing crates to a crate registry like crates.io.
///
/// see <https://doc.rust-lang.org/cargo/reference/registry-web-api.html#publish>
#[derive(Serialize, Deserialize)]
pub struct NewCrate {
    pub name: String,
    pub vers: String,
    pub deps: Vec<NewCrateDependency>,
    pub features: BTreeMap<String, Vec<String>>,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub documentation: Option<String>,
    pub homepage: Option<String>,
    pub readme: Option<String>,
    pub readme_file: Option<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub repository: Option<String>,
    pub badges: BTreeMap<String, BTreeMap<String, String>>,
    pub links: Option<String>,
    pub rust_version: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct NewCrateDependency {
    pub optional: bool,
    pub default_features: bool,
    pub name: String,
    pub features: Vec<String>,
    pub version_req: String,
    pub target: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_name_in_toml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bindep_target: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lib: bool,
}

#[derive(Deserialize)]
pub struct User {
    pub id: u32,
    pub login: String,
    pub avatar: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
}

pub struct Warnings {
    pub invalid_categories: Vec<String>,
    pub invalid_badges: Vec<String>,
    pub other: Vec<String>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Auth {
    Authorized,
    Unauthorized,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error returned when interacting with a registry.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error from libcurl.
    #[cfg(feature = "curl")]
    #[error(transparent)]
    Curl(#[from] curl::Error),

    /// Error from serializing the request payload and deserializing the
    /// response body (like response body didn't match expected structure).
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Error from IO. Mostly from reading the tarball to upload.
    #[error("failed to seek tarball")]
    Io(#[from] std::io::Error),

    /// Response body was not valid utf8.
    #[error("invalid response body from server")]
    Utf8(#[from] std::string::FromUtf8Error),

    /// Error from API response containing JSON field `errors.details`.
    #[error(
        "the remote server responded with an error{}: {}",
        status(*code),
        errors.join(", "),
    )]
    Api {
        code: u32,
        headers: Vec<String>,
        errors: Vec<String>,
    },

    /// Error from API response which didn't have pre-programmed `errors.details`.
    #[error(
        "failed to get a 200 OK response, got {code}\nheaders:\n\t{}\nbody:\n{body}",
        headers.join("\n\t"),
    )]
    Code {
        code: u32,
        headers: Vec<String>,
        body: String,
    },

    /// Reason why the token was invalid.
    #[error("{0}")]
    InvalidToken(&'static str),

    /// Server was unavailable and timed out. Happened when uploading a way
    /// too large tarball to crates.io.
    #[error(
        "Request timed out after 30 seconds. If you're trying to \
         upload a crate it may be too large. If the crate is under \
         10MB in size, you can email help@crates.io for assistance.\n\
         Total size was {0}."
    )]
    Timeout(u64),
}

// ---------------------------------------------------------------------------
// Registry — backend-agnostic HTTP client for crate registries
// ---------------------------------------------------------------------------

pub struct Registry {
    /// The base URL for issuing API requests.
    host: String,
    /// Optional authorization token.
    /// If None, commands requiring authorization will fail.
    token: Option<String>,
    /// Pluggable HTTP transport.
    handle: Box<dyn HttpHandle>,
    /// Whether to include the authorization token with all requests.
    auth_required: bool,
}

#[derive(Deserialize)]
struct ApiErrorList {
    errors: Vec<ApiError>,
}
#[derive(Deserialize)]
struct ApiError {
    detail: String,
}
#[derive(Deserialize)]
struct R {
    ok: bool,
}
#[derive(Deserialize)]
struct OwnerResponse {
    ok: bool,
    msg: String,
}
#[derive(Serialize)]
struct OwnersReq<'a> {
    users: &'a [&'a str],
}
#[derive(Deserialize)]
struct Users {
    users: Vec<User>,
}
#[derive(Deserialize)]
struct TotalCrates {
    total: u32,
}
#[derive(Deserialize)]
struct Crates {
    crates: Vec<Crate>,
    meta: TotalCrates,
}

impl Registry {
    /// Creates a new `Registry` with the given HTTP handle.
    ///
    /// ## Example (with curl)
    ///
    /// ```rust,ignore
    /// use curl::easy::Easy;
    /// use crates_io::{CurlHttpHandle, Registry};
    ///
    /// let mut easy = Easy::new();
    /// easy.useragent("my_crawler (example.com/info)");
    /// let handle = CurlHttpHandle(easy);
    /// let mut reg = Registry::new(String::from("https://crates.io"), None, handle, true);
    /// ```
    pub fn new(
        host: String,
        token: Option<String>,
        handle: impl HttpHandle + 'static,
        auth_required: bool,
    ) -> Registry {
        Registry {
            host,
            token,
            handle: Box::new(handle),
            auth_required,
        }
    }

    /// Creates a new `Registry` from a pre-boxed handle.
    pub fn new_handle(
        host: String,
        token: Option<String>,
        handle: Box<dyn HttpHandle>,
        auth_required: bool,
    ) -> Registry {
        Registry {
            host,
            token,
            handle,
            auth_required,
        }
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    fn token(&self) -> Result<&str> {
        let token = self.token.as_ref().ok_or_else(|| {
            Error::InvalidToken("no upload token found, please run `cargo login`")
        })?;
        check_token(token)?;
        Ok(token)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn host_is_crates_io(&self) -> bool {
        is_url_crates_io(&self.host)
    }

    pub fn add_owners(&mut self, krate: &str, owners: &[&str]) -> Result<String> {
        let body = serde_json::to_string(&OwnersReq { users: owners })?;
        let body = self.put(&format!("/crates/{}/owners", krate), body.as_bytes())?;
        assert!(serde_json::from_str::<OwnerResponse>(&body)?.ok);
        Ok(serde_json::from_str::<OwnerResponse>(&body)?.msg)
    }

    pub fn remove_owners(&mut self, krate: &str, owners: &[&str]) -> Result<()> {
        let body = serde_json::to_string(&OwnersReq { users: owners })?;
        let body = self.delete(&format!("/crates/{}/owners", krate), Some(body.as_bytes()))?;
        assert!(serde_json::from_str::<OwnerResponse>(&body)?.ok);
        Ok(())
    }

    pub fn list_owners(&mut self, krate: &str) -> Result<Vec<User>> {
        let body = self.get(&format!("/crates/{}/owners", krate))?;
        Ok(serde_json::from_str::<Users>(&body)?.users)
    }

    pub fn publish(&mut self, krate: &NewCrate, mut tarball: &File) -> Result<Warnings> {
        let json = serde_json::to_string(krate)?;
        // Prepare the body. The format of the upload request is:
        //
        //      <le u32 of json>
        //      <json request> (metadata for the package)
        //      <le u32 of tarball>
        //      <source tarball>

        // NOTE: This can be replaced with `stream_len` if it is ever stabilized.
        //
        // This checks the length using seeking instead of metadata, because
        // on some filesystems, getting the metadata will fail because
        // the file was renamed in ops::package.
        let tarball_len = tarball.seek(SeekFrom::End(0))?;
        tarball.seek(SeekFrom::Start(0))?;
        let header = {
            let mut w = Vec::new();
            w.extend(&(json.len() as u32).to_le_bytes());
            w.extend(json.as_bytes().iter().cloned());
            w.extend(&(tarball_len as u32).to_le_bytes());
            w
        };
        let mut body = Vec::from(header);
        tarball.read_to_end(&mut body)?;

        let url = format!("{}/api/v1/crates/new", self.host);
        let headers = vec![
            "Accept: application/json".to_string(),
            format!("Authorization: {}", self.token()?),
        ];

        let started = Instant::now();
        let resp = self
            .handle
            .request(&url, &headers, HttpMethod::Put, Some(&body))
            .map_err(|e| match e {
                Error::Code { code, .. }
                    if code == 503
                        && started.elapsed().as_secs() >= 29
                        && self.host_is_crates_io() =>
                {
                    Error::Timeout(tarball_len)
                }
                _ => e,
            })?;

        let body = parse_response(resp)?;

        let response = if body.is_empty() {
            "{}".parse()?
        } else {
            body.parse::<serde_json::Value>()?
        };

        let invalid_categories: Vec<String> = response
            .get("warnings")
            .and_then(|j| j.get("invalid_categories"))
            .and_then(|j| j.as_array())
            .map(|x| x.iter().flat_map(|j| j.as_str()).map(Into::into).collect())
            .unwrap_or_else(Vec::new);

        let invalid_badges: Vec<String> = response
            .get("warnings")
            .and_then(|j| j.get("invalid_badges"))
            .and_then(|j| j.as_array())
            .map(|x| x.iter().flat_map(|j| j.as_str()).map(Into::into).collect())
            .unwrap_or_else(Vec::new);

        let other: Vec<String> = response
            .get("warnings")
            .and_then(|j| j.get("other"))
            .and_then(|j| j.as_array())
            .map(|x| x.iter().flat_map(|j| j.as_str()).map(Into::into).collect())
            .unwrap_or_else(Vec::new);

        Ok(Warnings {
            invalid_categories,
            invalid_badges,
            other,
        })
    }

    pub fn search(&mut self, query: &str, limit: u32) -> Result<(Vec<Crate>, u32)> {
        let formatted_query = percent_encode(query.as_bytes(), NON_ALPHANUMERIC);
        let body = self.req(
            &format!("/crates?q={}&per_page={}", formatted_query, limit),
            None,
            Auth::Unauthorized,
        )?;

        let crates = serde_json::from_str::<Crates>(&body)?;
        Ok((crates.crates, crates.meta.total))
    }

    pub fn yank(&mut self, krate: &str, version: &str) -> Result<()> {
        let body = self.delete(&format!("/crates/{}/{}/yank", krate, version), None)?;
        assert!(serde_json::from_str::<R>(&body)?.ok);
        Ok(())
    }

    pub fn unyank(&mut self, krate: &str, version: &str) -> Result<()> {
        let body = self.put(&format!("/crates/{}/{}/unyank", krate, version), &[])?;
        assert!(serde_json::from_str::<R>(&body)?.ok);
        Ok(())
    }

    fn put(&mut self, path: &str, b: &[u8]) -> Result<String> {
        self.req(path, Some(b), Auth::Authorized)
    }

    fn get(&mut self, path: &str) -> Result<String> {
        self.req(path, None, Auth::Authorized)
    }

    fn delete(&mut self, path: &str, b: Option<&[u8]>) -> Result<String> {
        self.req_method(path, b, Auth::Authorized, HttpMethod::Delete)
    }

    fn req(&mut self, path: &str, body: Option<&[u8]>, authorized: Auth) -> Result<String> {
        let method = if body.is_some() {
            HttpMethod::Put
        } else {
            HttpMethod::Get
        };
        self.req_method(path, body, authorized, method)
    }

    fn req_method(
        &mut self,
        path: &str,
        body: Option<&[u8]>,
        authorized: Auth,
        method: HttpMethod,
    ) -> Result<String> {
        let url = format!("{}/api/v1{}", self.host, path);
        let mut headers = vec!["Accept: application/json".to_string()];
        if body.is_some() {
            headers.push("Content-Type: application/json".to_string());
        }
        if self.auth_required || authorized == Auth::Authorized {
            headers.push(format!("Authorization: {}", self.token()?));
        }

        let resp = self.handle.request(&url, &headers, method, body)?;
        parse_response(resp)
    }
}

fn parse_response(resp: HttpResponse) -> Result<String> {
    let body = String::from_utf8(resp.body)?;
    let errors = serde_json::from_str::<ApiErrorList>(&body)
        .ok()
        .map(|s| s.errors.into_iter().map(|s| s.detail).collect::<Vec<_>>());

    match (resp.status, errors) {
        (0, None) => Ok(body),
        (code, None) if is_success(code) => Ok(body),
        (code, Some(errors)) => Err(Error::Api {
            code,
            headers: resp.headers,
            errors,
        }),
        (code, None) => Err(Error::Code {
            code,
            headers: resp.headers,
            body,
        }),
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

fn is_success(code: u32) -> bool {
    code >= 200 && code < 300
}

fn status(code: u32) -> String {
    if is_success(code) {
        String::new()
    } else {
        let reason = reason(code);
        format!(" (status {code} {reason})")
    }
}

fn reason(code: u32) -> &'static str {
    // Taken from https://developer.mozilla.org/en-US/docs/Web/HTTP/Status
    match code {
        100 => "Continue",
        101 => "Switching Protocol",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choice",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Request Range Not Satisfiable",
        417 => "Expectation Failed",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "<unknown>",
    }
}

/// Returns `true` if the host of the given URL is "crates.io".
pub fn is_url_crates_io(url: &str) -> bool {
    Url::parse(url)
        .map(|u| u.host_str() == Some("crates.io"))
        .unwrap_or(false)
}

/// Checks if a token is valid or malformed.
///
/// This check is necessary to prevent sending tokens which create an invalid HTTP request.
/// It would be easier to check just for alphanumeric tokens, but we can't be sure that all
/// registries only create tokens in that format so that is as less restricted as possible.
pub fn check_token(token: &str) -> Result<()> {
    if token.is_empty() {
        return Err(Error::InvalidToken("please provide a non-empty token"));
    }
    if token.bytes().all(|b| {
        // This is essentially the US-ASCII limitation of
        // https://www.rfc-editor.org/rfc/rfc9110#name-field-values. That is,
        // visible ASCII characters (0x21-0x7e), space, and tab. We want to be
        // able to pass this in an HTTP header without encoding.
        b >= 32 && b < 127 || b == b'\t'
    }) {
        Ok(())
    } else {
        Err(Error::InvalidToken(
            "token contains invalid characters.\nOnly printable ISO-8859-1 characters \
             are allowed as it is sent in a HTTPS header.",
        ))
    }
}
