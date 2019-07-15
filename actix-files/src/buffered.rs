use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::{SystemTime, UNIX_EPOCH};
use std::path::PathBuf;
use futures::{Future, Async, Poll, Stream};
use bytes::Bytes;
use std::{io, cmp};

use actix_web::http::header::{self, EntityTag};
use actix_web::http::{ContentEncoding, Method, StatusCode};
use actix_web::middleware::BodyEncoding;
use actix_web::{web, Error, HttpResponse, Responder, HttpMessage, HttpRequest};
use actix_http::body::SizedStream;
use actix_web::error::{BlockingError, ErrorInternalServerError};

use crate::range::HttpRange;
use crate::named;

#[derive(Clone)]
pub struct BufferedFile {
    pub(crate) path: PathBuf, // Memorize original Path
    pub(crate) content: Vec<u8>, // Buffer Content
    pub(crate) etag: Option<EntityTag>, // Buffer ETAG

    pub(crate) content_type: mime::Mime, // same as in NamedFile
    pub(crate) content_disposition: header::ContentDisposition, // same as in NamedFile
    pub(crate) md: fs::Metadata, // same as in NamedFile
    pub modified: Option<SystemTime>, // same as in NamedFile
    pub(crate) encoding: Option<ContentEncoding>, // same as in NamedFile
    pub(crate) status_code: StatusCode, // same as in NamedFile
    pub(crate) flags: named::Flags, // same as in NamedFile
}

impl BufferedFile {

    /// Set response **Status Code**
    pub fn set_status_code(mut self, status: StatusCode) -> Self {
        self.status_code = status;
        self
    }

    /// Set the MIME Content-Type for serving this file. By default
    /// the Content-Type is inferred from the filename extension.
    #[inline]
    pub fn set_content_type(mut self, mime_type: mime::Mime) -> Self {
        self.content_type = mime_type;
        self
    }

    /// Set the Content-Disposition for serving this file. This allows
    /// changing the inline/attachment disposition as well as the filename
    /// sent to the peer. By default the disposition is `inline` for text,
    /// image, and video content types, and `attachment` otherwise, and
    /// the filename is taken from the path provided in the `open` method
    /// after converting it to UTF-8 using
    /// [to_string_lossy](https://doc.rust-lang.org/std/ffi/struct.OsStr.html#method.to_string_lossy).
    #[inline]
    pub fn set_content_disposition(mut self, cd: header::ContentDisposition) -> Self {
        self.content_disposition = cd;
        self
    }

    /// Set content encoding for serving this file
    #[inline]
    pub fn set_content_encoding(mut self, enc: ContentEncoding) -> Self {
        self.encoding = Some(enc);
        self
    }

    #[inline]
    ///Specifies whether to use ETag or not.
    ///
    ///Default is true.
    pub fn use_etag(mut self, value: bool) -> Self {
        self.flags.set(named::Flags::ETAG, value);
        self
    }

    #[inline]
    ///Specifies whether to use Last-Modified or not.
    ///
    ///Default is true.
    pub fn use_last_modified(mut self, value: bool) -> Self {
        self.flags.set(named::Flags::LAST_MD, value);
        self
    }

    pub(crate) fn last_modified(&self) -> Option<header::HttpDate> {
        self.modified.map(|mtime| mtime.into())
    }

    /// Re-Buffer BufferedFile
    /// TODO
    pub fn buffer(&mut self) -> &Self {
        println!("Re-Buffer self: {}", self.path.display());
        self
    }

    pub(crate) fn make_etag(bf: &mut BufferedFile) -> Option<EntityTag> {
        // This etag format is similar to Apache's.
        bf.modified.as_ref().map(|mtime| {
            let ino = {
                #[cfg(unix)]
                {
                    bf.md.ino()
                }
                #[cfg(not(unix))]
                {
                    0
                }
            };

            let dur = mtime
                .duration_since(UNIX_EPOCH)
                .expect("modification time must be after epoch");
            header::EntityTag::strong(format!(
                "{:x}:{:x}:{:x}:{:x}",
                ino,
                bf.md.len(),
                dur.as_secs(),
                dur.subsec_nanos()
            ))
        })
    }
}

impl Responder for BufferedFile {
    type Error = Error;
    type Future = Result<HttpResponse, Error>;

    fn respond_to(self, req: &HttpRequest) -> Self::Future {

        // Check for non-200 Status Code
        // If not OK then steam back complete file
        if self.status_code != StatusCode::OK {
            let mut resp = HttpResponse::build(self.status_code);
            resp.set(header::ContentType(self.content_type.clone()))
                .header(
                    header::CONTENT_DISPOSITION,
                    self.content_disposition.to_string(),
                );
            if let Some(current_encoding) = self.encoding {
                resp.encoding(current_encoding);
            }
            let reader = ChunkedReadFile {
                size: self.md.len(),
                offset: 0,
                content: self.content.clone(),
                counter: 0,
                fut: None,
            };
            return Ok(resp.streaming(reader));
        }

        // Check for correct HEADER Method
        // At wrong header, anser back with MethodNotAllowed 405
        match req.method() {
            &Method::HEAD | &Method::GET => (),
            _ => {
                return Ok(HttpResponse::MethodNotAllowed()
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header(header::ALLOW, "GET, HEAD")
                    .body("This resource only supports GET and HEAD."));
            }
        }

        let last_modified = if self.flags.contains(named::Flags::LAST_MD) {
            self.last_modified()
        } else {
            None
        };

        // check preconditions
        // precondition_failed: there is no
        let precondition_failed = if !any_match(self.etag.as_ref(), req) {
            true
        } else if let (Some(ref m), Some(header::IfUnmodifiedSince(ref since))) =
            (last_modified, req.get_header())
        {
            let t1: SystemTime = m.clone().into();
            let t2: SystemTime = since.clone().into();
            match (t1.duration_since(UNIX_EPOCH), t2.duration_since(UNIX_EPOCH)) {
                (Ok(t1), Ok(t2)) => t1 > t2,
                _ => false,
            }
        } else {
            false
        };

        // check last modified
        let not_modified = if !none_match(self.etag.as_ref(), req) {
            true
        } else if req.headers().contains_key(&header::IF_NONE_MATCH) {
            false
        } else if let (Some(ref m), Some(header::IfModifiedSince(ref since))) =
            (last_modified, req.get_header())
        {
            let t1: SystemTime = m.clone().into();
            let t2: SystemTime = since.clone().into();
            match (t1.duration_since(UNIX_EPOCH), t2.duration_since(UNIX_EPOCH)) {
                (Ok(t1), Ok(t2)) => t1 <= t2,
                _ => false,
            }
        } else {
            false
        };

        let mut resp = HttpResponse::build(self.status_code);
        resp.set(header::ContentType(self.content_type.clone()))
            .header(
                header::CONTENT_DISPOSITION,
                self.content_disposition.to_string(),
            );
        // default compressing
        if let Some(current_encoding) = self.encoding {
            resp.encoding(current_encoding);
        }

        resp.if_some(last_modified, |lm, resp| {
            resp.set(header::LastModified(lm));
        })
        .if_some(self.etag, |etag, resp| {
            resp.set(header::ETag(etag));
        });

        resp.header(header::ACCEPT_RANGES, "bytes");

        let mut length = self.md.len();
        let mut offset = 0;

        // check for range header
        if let Some(ranges) = req.headers().get(&header::RANGE) {
            if let Ok(rangesheader) = ranges.to_str() {
                if let Ok(rangesvec) = HttpRange::parse(rangesheader, length) {
                    length = rangesvec[0].length;
                    offset = rangesvec[0].start;
                    resp.encoding(ContentEncoding::Identity);
                    resp.header(
                        header::CONTENT_RANGE,
                        format!(
                            "bytes {}-{}/{}",
                            offset,
                            offset + length - 1,
                            self.md.len()
                        ),
                    );
                } else {
                    resp.header(header::CONTENT_RANGE, format!("bytes */{}", length));
                    return Ok(resp.status(StatusCode::RANGE_NOT_SATISFIABLE).finish());
                };
            } else {
                return Ok(resp.status(StatusCode::BAD_REQUEST).finish());
            };
        };

        if precondition_failed {
            return Ok(resp.status(StatusCode::PRECONDITION_FAILED).finish());
        } else if not_modified {
            return Ok(resp.status(StatusCode::NOT_MODIFIED).finish());
        }

        let reader = ChunkedReadFile {
            offset,
            size: length,
            content: self.content.clone(),
            counter: 0,
            fut: None,
        };
        if offset != 0 || length != self.md.len() {
            return Ok(resp.status(StatusCode::PARTIAL_CONTENT).streaming(reader));
        };
        Ok(resp.body(SizedStream::new(length, reader)))
    }
}

/// Returns true if `req` has no `If-Match` header or one which matches `etag`.
fn any_match(etag: Option<&header::EntityTag>, req: &HttpRequest) -> bool {
    match req.get_header::<header::IfMatch>() {
        None | Some(header::IfMatch::Any) => true,
        Some(header::IfMatch::Items(ref items)) => {
            if let Some(some_etag) = etag {
                for item in items {
                    if item.strong_eq(some_etag) {
                        return true;
                    }
                }
            }
            false
        }
    }
}

/// Returns true if `req` doesn't have an `If-None-Match` header matching `req`.
fn none_match(etag: Option<&header::EntityTag>, req: &HttpRequest) -> bool {
    match req.get_header::<header::IfNoneMatch>() {
        Some(header::IfNoneMatch::Any) => false,
        Some(header::IfNoneMatch::Items(ref items)) => {
            if let Some(some_etag) = etag {
                for item in items {
                    if item.weak_eq(some_etag) {
                        return false;
                    }
                }
            }
            true
        }
        None => true,
    }
}

/// Stream struct
pub struct ChunkedReadFile {
    size: u64,
    offset: u64,
    content: Vec<u8>,
    fut: Option<Box<Future<Item = (Bytes), Error = BlockingError<io::Error>>>>,
    counter: u64,
}

fn handle_error(err: BlockingError<io::Error>) -> Error {
    match err {
        BlockingError::Error(err) => err.into(),
        BlockingError::Canceled => ErrorInternalServerError("Unexpected error").into(),
    }
}

impl Stream for ChunkedReadFile {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Bytes>, Error> {
        if self.fut.is_some() {
            return match self.fut.as_mut().unwrap().poll().map_err(handle_error)? {
                Async::Ready(bytes) => {
                    self.fut.take();
                    self.offset += bytes.len() as u64;
                    self.counter += bytes.len() as u64;
                    Ok(Async::Ready(Some(bytes)))
                }
                Async::NotReady => Ok(Async::NotReady),
            };
        }

        let size = self.size as usize;
        let offset = self.offset as usize;
        let counter = self.counter as usize;
        

        if size == counter {
            Ok(Async::Ready(None)) // ??? Ready(None) actually determins the end of the stream ???
        } else {

            let max_bytes: usize = cmp::min(size.saturating_sub(counter), 65_536) as usize;
            let mut buf = vec![0; max_bytes]; //Vec::with_capacity(max_bytes);
            let content_len = self.content.len();

            // Make sure not accessing Vec out of range
            if !(offset + size > content_len) {
                //log::debug!("### COPY ### Buf Capacity: {} content Capacity: {}", buf.capacity(), self.content.capacity());
                buf.copy_from_slice(&self.content[offset..=(offset + max_bytes - 1)]);
            }

            self.fut = Some(Box::new(web::block(move || {
                
                // Make sure not accessing Vec out of range
                if offset + size > content_len {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                } 

                Ok(Bytes::from(buf))
            })));
            self.poll()
        }
    }
}
