// src/io/itarbundle.rs -- I/O on files in an indexed tar file "bundle"
// Copyright 2017-2019 the Tectonic Project
// Licensed under the MIT License.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Cursor, Read};

use super::{Bundle, InputHandle, InputOrigin, IoProvider, OpenResult};
use crate::io::download::{self, Client, Response, StatusCode};
use crate::errors::{Error, ErrorKind, Result, ResultExt};
use crate::status::StatusBackend;
use crate::{tt_note, tt_warning};

// A simple way to read chunks out of a big seekable byte stream. You could
// implement this for io::File pretty trivially but that's not currently
// needed.

pub trait RangeRead {
    type InnerRead: Read;

    fn read_range(&mut self, offset: u64, length: usize) -> Result<Self::InnerRead>;
}

pub struct HttpRangeReader {
    url: String,
    client: Client,
}

impl HttpRangeReader {
    pub fn new(url: &str) -> HttpRangeReader {
        HttpRangeReader {
            url: url.to_owned(),
            client: Client::new(),
        }
    }
}

impl RangeRead for HttpRangeReader {
    type InnerRead = Response;

    fn read_range(&mut self, offset: u64, length: usize) -> Result<Response> {
        let end_inclusive = offset + length as u64 - 1;
        let res = download::get_range(&mut self.client, &self.url, offset, end_inclusive)?;

        if res.status() != StatusCode::PARTIAL_CONTENT {
            return Err(Error::from(ErrorKind::UnexpectedHttpResponse(
                self.url.clone(),
                res.status(),
            )))
            .chain_err(|| format!("read range expected {}", StatusCode::PARTIAL_CONTENT));
        }

        Ok(res)
    }
}

// The IoProvider. We jump through some hoops so that web-based bundles can
// be created without immediately connecting to the network.

pub trait ITarIoFactory {
    type IndexReader: Read;
    type DataReader: RangeRead;

    fn get_index(&mut self, status: &mut dyn StatusBackend) -> Result<Self::IndexReader>;
    fn get_data(&self) -> Result<Self::DataReader>;
    fn report_fetch(&self, name: &OsStr, status: &mut dyn StatusBackend);
}

struct FileInfo {
    offset: u64,
    length: u64,
}

pub struct ITarBundle<F: ITarIoFactory> {
    factory: F,
    data: Option<F::DataReader>,
    index: HashMap<OsString, FileInfo>,
}

impl<F: ITarIoFactory> ITarBundle<F> {
    fn construct(factory: F) -> ITarBundle<F> {
        ITarBundle {
            factory,
            data: None,
            index: HashMap::new(),
        }
    }

    fn ensure_loaded(&mut self, status: &mut dyn StatusBackend) -> Result<()> {
        if self.data.is_some() {
            return Ok(());
        }

        // We need to initialize. First, the index ...

        let index = self.factory.get_index(status)?;
        let br = BufReader::new(index);

        for res in br.lines() {
            let line = res?;
            let bits = line.split_whitespace().collect::<Vec<_>>();

            if bits.len() < 3 {
                continue; // TODO: preserve the warning info or something!
            }

            self.index.insert(
                OsString::from(bits[0]),
                FileInfo {
                    offset: bits[1].parse::<u64>()?,
                    length: bits[2].parse::<u64>()?,
                },
            );
        }

        // ... then, the data reader.

        self.data = Some(self.factory.get_data()?);
        Ok(())
    }
}

impl<F: ITarIoFactory> IoProvider for ITarBundle<F> {
    fn input_open_name(
        &mut self,
        name: &OsStr,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        if let Err(e) = self.ensure_loaded(status) {
            return OpenResult::Err(e);
        }

        // In principle it'd be cool to return a handle right to the HTTP
        // response, but those can't be seekable, and doing so introduces
        // lifetime-related issues. So for now we just slurp the whole thing
        // into RAM.

        let info = match self.index.get(name) {
            Some(i) => i,
            None => return OpenResult::NotAvailable,
        };

        self.factory.report_fetch(name, status);

        // When fetching a bunch of resource files (i.e., on the first
        // invocation), bintray will sometimes drop connections. The error
        // manifests itself in a way that has a not-so-nice user experience.
        // Our solution: retry the HTTP a few times in case it was a transient
        // problem.

        let mut buf = Vec::with_capacity(info.length as usize);
        let mut overall_failed = true;
        let mut any_failed = false;

        for _ in 0..download::MAX_HTTP_ATTEMPTS {
            let mut stream = match self
                .data
                .as_mut()
                .unwrap()
                .read_range(info.offset, info.length as usize)
            {
                Ok(r) => r,
                Err(e) => {
                    tt_warning!(status, "failure requesting \"{}\" from network", name.to_string_lossy(); e);
                    any_failed = true;
                    continue;
                }
            };

            if let Err(e) = stream.read_to_end(&mut buf) {
                tt_warning!(status, "failure downloading \"{}\" from network", name.to_string_lossy(); e.into());
                any_failed = true;
                continue;
            }

            overall_failed = false;
            break;
        }

        if overall_failed {
            return OpenResult::Err(
                ErrorKind::Msg(format!(
                    "failed to retrieve \"{}\" from the network; \
                     this most probably is not Tectonic's fault \
                     -- please check your network connection.",
                    name.to_string_lossy()
                ))
                .into(),
            );
        } else if any_failed {
            tt_note!(status, "download succeeded after retry");
        }

        OpenResult::Ok(InputHandle::new(name, Cursor::new(buf), InputOrigin::Other))
    }
}

impl<F: ITarIoFactory> Bundle for ITarBundle<F> {}

pub struct HttpITarIoFactory {
    url: String,
}

impl ITarIoFactory for HttpITarIoFactory {
    type IndexReader = GzDecoder<Response>;
    type DataReader = HttpRangeReader;

    fn get_index(&mut self, status: &mut dyn StatusBackend) -> Result<GzDecoder<Response>> {
        tt_note!(status, "indexing {}", self.url);

        let res = download::head(&self.url)?;

        if !(res.status().is_success() || res.status() == StatusCode::FOUND) {
            return Err(Error::from(ErrorKind::UnexpectedHttpResponse(
                self.url.clone(),
                res.status(),
            )))
            .chain_err(|| "couldn\'t probe".to_string());
        }

        let final_url = res.url().clone().into_string();

        if final_url != self.url {
            tt_note!(status, "resolved to {}", final_url);
            self.url = final_url;
        }

        // Now let's actually go for the index.

        let mut index_url = self.url.clone();
        index_url.push_str(".index.gz");

        let res = download::get(&index_url)?;
        if !res.status().is_success() {
            return Err(Error::from(ErrorKind::UnexpectedHttpResponse(
                index_url.clone(),
                res.status(),
            )))
            .chain_err(|| "couldn\'t fetch".to_string());
        }

        Ok(GzDecoder::new(res))
    }

    fn get_data(&self) -> Result<HttpRangeReader> {
        Ok(HttpRangeReader::new(&self.url))
    }

    fn report_fetch(&self, name: &OsStr, status: &mut dyn StatusBackend) {
        tt_note!(status, "downloading {}", name.to_string_lossy());
    }
}

impl ITarBundle<HttpITarIoFactory> {
    pub fn new(url: &str) -> ITarBundle<HttpITarIoFactory> {
        Self::construct(HttpITarIoFactory {
            url: url.to_owned(),
        })
    }
}
