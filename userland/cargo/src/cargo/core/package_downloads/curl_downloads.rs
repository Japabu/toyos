//! Download backend using curl's multi interface for parallel HTTP downloads.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::mem;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use curl::easy::Easy;
use curl::multi::{EasyHandle, Multi};
use tracing::debug;

use crate::core::package::{Package, PackageSet};
use crate::core::PackageId;
use crate::sources::source::MaybePackage;
use crate::util::cache_lock::{CacheLock, CacheLockMode};
use crate::util::errors::{CargoResult, HttpNotSuccessful};
use crate::util::interning::InternedString;
use crate::util::network::http::HttpTimeout;
use crate::util::network::http_curl::http_handle_and_timeout;
use crate::util::network::retry::{Retry, RetryResult};
use crate::util::network::sleep::SleepTracker;
use crate::util::{self, GlobalContext, HumanBytes, Progress, ProgressStyle, internal};

/// Backend state for curl-based downloads.
pub struct DownloadState {
    pub(crate) multi: Multi,
    pub(crate) multiplexing: bool,
}

impl DownloadState {
    pub fn new(gctx: &GlobalContext) -> CargoResult<Self> {
        // We've enabled the `http2` feature of `curl` in Cargo, so treat
        // failures here as fatal as it would indicate a build-time problem.
        let mut multi = Multi::new();
        let multiplexing = gctx.http_config()?.multiplexing.unwrap_or(true);
        multi
            .pipelining(false, multiplexing)
            .context("failed to enable multiplexing/pipelining in curl")?;

        // let's not flood crates.io with connections
        multi.set_max_host_connections(2)?;

        Ok(DownloadState {
            multi,
            multiplexing,
        })
    }
}

/// Helper for downloading crates.
pub struct Downloads<'a, 'gctx> {
    pub(crate) set: &'a PackageSet<'gctx>,
    /// When a download is started, it is added to this map. The key is a
    /// "token" (see `Download::token`). It is removed once the download is
    /// finished.
    pending: HashMap<usize, (Download<'gctx>, EasyHandle)>,
    /// Set of packages currently being downloaded. This should stay in sync
    /// with `pending`.
    pending_ids: HashSet<PackageId>,
    /// Downloads that have failed and are waiting to retry again later.
    sleeping: SleepTracker<(Download<'gctx>, Easy)>,
    /// The final result of each download. A pair `(token, result)`. This is a
    /// temporary holding area, needed because curl can report multiple
    /// downloads at once, but the main loop (`wait`) is written to only
    /// handle one at a time.
    results: Vec<(usize, Result<(), curl::Error>)>,
    /// The next ID to use for creating a token (see `Download::token`).
    next: usize,
    /// Progress bar.
    progress: RefCell<Option<Progress<'gctx>>>,
    /// Number of downloads that have successfully finished.
    downloads_finished: usize,
    /// Total bytes for all successfully downloaded packages.
    downloaded_bytes: u64,
    /// Size (in bytes) and package name of the largest downloaded package.
    largest: (u64, InternedString),
    /// Time when downloading started.
    start: Instant,
    /// Indicates *all* downloads were successful.
    pub success: bool,

    /// Timeout management, both of timeout thresholds as well as whether or not
    /// our connection has timed out (and accompanying message if it has).
    ///
    /// Note that timeout management is done manually here instead of in libcurl
    /// because we want to apply timeouts to an entire batch of operations, not
    /// any one particular single operation.
    timeout: HttpTimeout,
    /// Last time bytes were received.
    updated_at: Cell<Instant>,
    /// This is a slow-speed check. It is reset to `now + timeout_duration`
    /// every time at least `threshold` bytes are received. If the current
    /// time ever exceeds `next_speed_check`, then give up and report a
    /// timeout error.
    next_speed_check: Cell<Instant>,
    /// This is the slow-speed threshold byte count. It starts at the
    /// configured threshold value (default 10), and is decremented by the
    /// number of bytes received in each chunk. If it is <= zero, the
    /// threshold has been met and data is being received fast enough not to
    /// trigger a timeout; reset `next_speed_check` and set this back to the
    /// configured threshold.
    next_speed_check_bytes_threshold: Cell<u64>,
    /// Global filesystem lock to ensure only one Cargo is downloading at a
    /// time.
    _lock: CacheLock<'gctx>,
}

struct Download<'gctx> {
    /// The token for this download, used as the key of the `Downloads::pending` map
    /// and stored in `EasyHandle` as well.
    token: usize,

    /// The package that we're downloading.
    id: PackageId,

    /// Actual downloaded data, updated throughout the lifetime of this download.
    data: RefCell<Vec<u8>>,

    /// HTTP headers for debugging.
    headers: RefCell<Vec<String>>,

    /// The URL that we're downloading from, cached here for error messages and
    /// reenqueuing.
    url: String,

    /// A descriptive string to print when we've finished downloading this crate.
    descriptor: String,

    /// Statistics updated from the progress callback in libcurl.
    total: Cell<u64>,
    current: Cell<u64>,

    /// The moment we started this transfer at.
    start: Instant,
    timed_out: Cell<Option<String>>,

    /// Logic used to track retrying this download if it's a spurious failure.
    retry: Retry<'gctx>,
}

impl<'a, 'gctx> Downloads<'a, 'gctx> {
    pub fn new(set: &'a PackageSet<'gctx>) -> CargoResult<Self> {
        let timeout = HttpTimeout::new(set.gctx)?;
        Ok(Downloads {
            start: Instant::now(),
            set,
            next: 0,
            pending: HashMap::new(),
            pending_ids: HashSet::new(),
            sleeping: SleepTracker::new(),
            results: Vec::new(),
            progress: RefCell::new(Some(Progress::with_style(
                "Downloading",
                ProgressStyle::Ratio,
                set.gctx,
            ))),
            downloads_finished: 0,
            downloaded_bytes: 0,
            largest: (0, "".into()),
            success: false,
            updated_at: Cell::new(Instant::now()),
            timeout,
            next_speed_check: Cell::new(Instant::now()),
            next_speed_check_bytes_threshold: Cell::new(0),
            _lock: set
                .gctx
                .acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?,
        })
    }

    /// Starts to download the package for the `id` specified.
    ///
    /// Returns `None` if the package is queued up for download and will
    /// eventually be returned from `wait_for_download`. Returns `Some(pkg)` if
    /// the package is ready and doesn't need to be downloaded.
    #[tracing::instrument(skip_all)]
    pub fn start(&mut self, id: PackageId) -> CargoResult<Option<&'a Package>> {
        self.start_inner(id)
            .with_context(|| format!("failed to download `{}`", id))
    }

    fn start_inner(&mut self, id: PackageId) -> CargoResult<Option<&'a Package>> {
        // First up see if we've already cached this package, in which case
        // there's nothing to do.
        let slot = self
            .set
            .packages
            .get(&id)
            .ok_or_else(|| internal(format!("couldn't find `{}` in package set", id)))?;
        if let Some(pkg) = slot.get() {
            return Ok(Some(pkg));
        }

        // Ask the original source for this `PackageId` for the corresponding
        // package. That may immediately come back and tell us that the package
        // is ready, or it could tell us that it needs to be downloaded.
        let mut sources = self.set.sources.borrow_mut();
        let source = sources
            .get_mut(id.source_id())
            .ok_or_else(|| internal(format!("couldn't find source for `{}`", id)))?;
        let pkg = source
            .download(id)
            .context("unable to get packages from source")?;
        let (url, descriptor, authorization) = match pkg {
            MaybePackage::Ready(pkg) => {
                debug!("{} doesn't need a download", id);
                assert!(slot.set(pkg).is_ok());
                return Ok(Some(slot.get().unwrap()));
            }
            MaybePackage::Download {
                url,
                descriptor,
                authorization,
            } => (url, descriptor, authorization),
        };

        // Ok we're going to download this crate, so let's set up all our
        // internal state and hand off an `Easy` handle to our libcurl `Multi`
        // handle. This won't actually start the transfer, but later it'll
        // happen during `wait_for_download`
        let token = self.next;
        self.next += 1;
        debug!(target: "network", "downloading {} as {}", id, token);
        assert!(self.pending_ids.insert(id));

        let (mut handle, _timeout) = http_handle_and_timeout(self.set.gctx)?;
        handle.get(true)?;
        handle.url(&url)?;
        handle.follow_location(true)?; // follow redirects

        // Add authorization header.
        if let Some(authorization) = authorization {
            let mut headers = curl::easy::List::new();
            headers.append(&format!("Authorization: {}", authorization))?;
            handle.http_headers(headers)?;
        }

        // Enable HTTP/2 if possible.
        crate::try_old_curl_http2_pipewait!(self.set.dl_state.multiplexing, handle);

        handle.write_function(move |buf| {
            debug!(target: "network", "{} - {} bytes of data", token, buf.len());
            tls::with(|downloads| {
                if let Some(downloads) = downloads {
                    downloads.pending[&token]
                        .0
                        .data
                        .borrow_mut()
                        .extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        })?;
        handle.header_function(move |data| {
            tls::with(|downloads| {
                if let Some(downloads) = downloads {
                    // Headers contain trailing \r\n, trim them to make it easier
                    // to work with.
                    let h = String::from_utf8_lossy(data).trim().to_string();
                    downloads.pending[&token].0.headers.borrow_mut().push(h);
                }
            });
            true
        })?;

        handle.progress(true)?;
        handle.progress_function(move |dl_total, dl_cur, _, _| {
            tls::with(|downloads| match downloads {
                Some(d) => d.progress(token, dl_total as u64, dl_cur as u64),
                None => false,
            })
        })?;

        // If the progress bar isn't enabled then it may be awhile before the
        // first crate finishes downloading so we inform immediately that we're
        // downloading crates here.
        if self.downloads_finished == 0
            && self.pending.is_empty()
            && !self.progress.borrow().as_ref().unwrap().is_enabled()
        {
            self.set.gctx.shell().status("Downloading", "crates ...")?;
        }

        let dl = Download {
            token,
            data: RefCell::new(Vec::new()),
            headers: RefCell::new(Vec::new()),
            id,
            url,
            descriptor,
            total: Cell::new(0),
            current: Cell::new(0),
            start: Instant::now(),
            timed_out: Cell::new(None),
            retry: Retry::new(self.set.gctx)?,
        };
        self.enqueue(dl, handle)?;
        self.tick(WhyTick::DownloadStarted)?;

        Ok(None)
    }

    /// Returns the number of crates that are still downloading.
    pub fn remaining(&self) -> usize {
        self.pending.len() + self.sleeping.len()
    }

    /// Blocks the current thread waiting for a package to finish downloading.
    ///
    /// This method will wait for a previously enqueued package to finish
    /// downloading and return a reference to it after it's done downloading.
    ///
    /// # Panics
    ///
    /// This function will panic if there are no remaining downloads.
    #[tracing::instrument(skip_all)]
    pub fn wait(&mut self) -> CargoResult<&'a Package> {
        let (dl, data) = loop {
            assert_eq!(self.pending.len(), self.pending_ids.len());
            let (token, result) = self.wait_for_curl()?;
            debug!(target: "network", "{} finished with {:?}", token, result);

            let (mut dl, handle) = self
                .pending
                .remove(&token)
                .expect("got a token for a non-in-progress transfer");
            let data = mem::take(&mut *dl.data.borrow_mut());
            let headers = mem::take(&mut *dl.headers.borrow_mut());
            let mut handle = self.set.dl_state.multi.remove(handle)?;
            self.pending_ids.remove(&dl.id);

            // Check if this was a spurious error. If it was a spurious error
            // then we want to re-enqueue our request for another attempt and
            // then we wait for another request to finish.
            let ret = {
                let timed_out = &dl.timed_out;
                let url = &dl.url;
                dl.retry.r#try(|| {
                    if let Err(e) = result {
                        // If this error is "aborted by callback" then that's
                        // probably because our progress callback aborted due to
                        // a timeout. We'll find out by looking at the
                        // `timed_out` field, looking for a descriptive message.
                        // If one is found we switch the error code (to ensure
                        // it's flagged as spurious) and then attach our extra
                        // information to the error.
                        if !e.is_aborted_by_callback() {
                            return Err(e.into());
                        }

                        return Err(match timed_out.replace(None) {
                            Some(msg) => {
                                let code = curl_sys::CURLE_OPERATION_TIMEDOUT;
                                let mut err = curl::Error::new(code);
                                err.set_extra(msg);
                                err
                            }
                            None => e,
                        }
                        .into());
                    }

                    let code = handle.response_code()?;
                    if code != 200 && code != 0 {
                        return Err(HttpNotSuccessful::new_from_handle(
                            &mut handle,
                            &url,
                            data,
                            headers,
                        )
                        .into());
                    }
                    Ok(data)
                })
            };
            match ret {
                RetryResult::Success(data) => break (dl, data),
                RetryResult::Err(e) => {
                    return Err(e.context(format!("failed to download from `{}`", dl.url)));
                }
                RetryResult::Retry(sleep) => {
                    debug!(target: "network", "download retry {} for {sleep}ms", dl.url);
                    self.sleeping.push(sleep, (dl, handle));
                }
            }
        };

        // If the progress bar isn't enabled then we still want to provide some
        // semblance of progress of how we're downloading crates, and if the
        // progress bar is enabled this provides a good log of what's happening.
        self.progress.borrow_mut().as_mut().unwrap().clear();
        self.set.gctx.shell().status("Downloaded", &dl.descriptor)?;

        self.downloads_finished += 1;
        self.downloaded_bytes += dl.total.get();
        if dl.total.get() > self.largest.0 {
            self.largest = (dl.total.get(), dl.id.name());
        }

        // We're about to synchronously extract the crate below. While we're
        // doing that our download progress won't actually be updated, nor do we
        // have a great view into the progress of the extraction. Let's prepare
        // the user for this CPU-heavy step if it looks like it'll take some
        // time to do so.
        let kib_400 = 1024 * 400;
        if dl.total.get() < kib_400 {
            self.tick(WhyTick::DownloadFinished)?;
        } else {
            self.tick(WhyTick::Extracting(&dl.id.name()))?;
        }

        // Inform the original source that the download is finished which
        // should allow us to actually get the package and fill it in now.
        let mut sources = self.set.sources.borrow_mut();
        let source = sources
            .get_mut(dl.id.source_id())
            .ok_or_else(|| internal(format!("couldn't find source for `{}`", dl.id)))?;
        let start = Instant::now();
        let pkg = source.finish_download(dl.id, data)?;

        // Assume that no time has passed while we were calling
        // `finish_download`, update all speed checks and timeout limits of all
        // active downloads to make sure they don't fire because of a slowly
        // extracted tarball.
        let finish_dur = start.elapsed();
        self.updated_at.set(self.updated_at.get() + finish_dur);
        self.next_speed_check
            .set(self.next_speed_check.get() + finish_dur);

        let slot = &self.set.packages[&dl.id];
        assert!(slot.set(pkg).is_ok());
        Ok(slot.get().unwrap())
    }

    fn enqueue(&mut self, dl: Download<'gctx>, handle: Easy) -> CargoResult<()> {
        let mut handle = self.set.dl_state.multi.add(handle)?;
        let now = Instant::now();
        handle.set_token(dl.token)?;
        self.updated_at.set(now);
        self.next_speed_check.set(now + self.timeout.dur);
        self.next_speed_check_bytes_threshold
            .set(u64::from(self.timeout.low_speed_limit));
        dl.timed_out.set(None);
        dl.current.set(0);
        dl.total.set(0);
        self.pending.insert(dl.token, (dl, handle));
        Ok(())
    }

    /// Block, waiting for curl. Returns a token and a `Result` for that token
    /// (`Ok` means the download successfully finished).
    fn wait_for_curl(&mut self) -> CargoResult<(usize, Result<(), curl::Error>)> {
        // This is the main workhorse loop. We use libcurl's portable `wait`
        // method to actually perform blocking. This isn't necessarily too
        // efficient in terms of fd management, but we should only be juggling
        // a few anyway.
        //
        // Here we start off by asking the `multi` handle to do some work via
        // the `perform` method. This will actually do I/O work (non-blocking)
        // and attempt to make progress. Afterwards we ask about the `messages`
        // contained in the handle which will inform us if anything has finished
        // transferring.
        //
        // If we've got a finished transfer after all that work we break out
        // and process the finished transfer at the end. Otherwise we need to
        // actually block waiting for I/O to happen, which we achieve with the
        // `wait` method on `multi`.
        loop {
            self.add_sleepers()?;
            let n = tls::set(self, || {
                self.set
                    .dl_state
                    .multi
                    .perform()
                    .context("failed to perform http requests")
            })?;
            debug!(target: "network", "handles remaining: {}", n);
            let results = &mut self.results;
            let pending = &self.pending;
            self.set.dl_state.multi.messages(|msg| {
                let token = msg.token().expect("failed to read token");
                let handle = &pending[&token].1;
                if let Some(result) = msg.result_for(handle) {
                    results.push((token, result));
                } else {
                    debug!(target: "network", "message without a result (?)");
                }
            });

            if let Some(pair) = results.pop() {
                break Ok(pair);
            }
            assert_ne!(self.remaining(), 0);
            if self.pending.is_empty() {
                let delay = self.sleeping.time_to_next().unwrap();
                debug!(target: "network", "sleeping main thread for {delay:?}");
                std::thread::sleep(delay);
            } else {
                let min_timeout = Duration::new(1, 0);
                let timeout = self
                    .set
                    .dl_state
                    .multi
                    .get_timeout()?
                    .unwrap_or(min_timeout);
                let timeout = timeout.min(min_timeout);
                self.set
                    .dl_state
                    .multi
                    .wait(&mut [], timeout)
                    .context("failed to wait on curl `Multi`")?;
            }
        }
    }

    fn add_sleepers(&mut self) -> CargoResult<()> {
        for (dl, handle) in self.sleeping.to_retry() {
            self.pending_ids.insert(dl.id);
            self.enqueue(dl, handle)?;
        }
        Ok(())
    }

    fn progress(&self, token: usize, total: u64, cur: u64) -> bool {
        let dl = &self.pending[&token].0;
        dl.total.set(total);
        let now = Instant::now();
        if cur > dl.current.get() {
            let delta = cur - dl.current.get();
            let threshold = self.next_speed_check_bytes_threshold.get();

            dl.current.set(cur);
            self.updated_at.set(now);

            if delta >= threshold {
                self.next_speed_check.set(now + self.timeout.dur);
                self.next_speed_check_bytes_threshold
                    .set(u64::from(self.timeout.low_speed_limit));
            } else {
                self.next_speed_check_bytes_threshold.set(threshold - delta);
            }
        }
        if self.tick(WhyTick::DownloadUpdate).is_err() {
            return false;
        }

        // If we've spent too long not actually receiving any data we time out.
        if now > self.updated_at.get() + self.timeout.dur {
            self.updated_at.set(now);
            let msg = format!(
                "failed to download any data for `{}` within {}s",
                dl.id,
                self.timeout.dur.as_secs()
            );
            dl.timed_out.set(Some(msg));
            return false;
        }

        // If we reached the point in time that we need to check our speed
        // limit, see if we've transferred enough data during this threshold. If
        // it fails this check then we fail because the download is going too
        // slowly.
        if now >= self.next_speed_check.get() {
            self.next_speed_check.set(now + self.timeout.dur);
            assert!(self.next_speed_check_bytes_threshold.get() > 0);
            let msg = format!(
                "download of `{}` failed to transfer more \
                 than {} bytes in {}s",
                dl.id,
                self.timeout.low_speed_limit,
                self.timeout.dur.as_secs()
            );
            dl.timed_out.set(Some(msg));
            return false;
        }

        true
    }

    fn tick(&self, why: WhyTick<'_>) -> CargoResult<()> {
        let mut progress = self.progress.borrow_mut();
        let progress = progress.as_mut().unwrap();

        if let WhyTick::DownloadUpdate = why {
            if !progress.update_allowed() {
                return Ok(());
            }
        }
        let pending = self.remaining();
        let mut msg = if pending == 1 {
            format!("{} crate", pending)
        } else {
            format!("{} crates", pending)
        };
        match why {
            WhyTick::Extracting(krate) => {
                msg.push_str(&format!(", extracting {} ...", krate));
            }
            _ => {
                let mut dur = Duration::new(0, 0);
                let mut remaining = 0;
                for (dl, _) in self.pending.values() {
                    dur += dl.start.elapsed();
                    // If the total/current look weird just throw out the data
                    // point, sounds like curl has more to learn before we have
                    // the true information.
                    if dl.total.get() >= dl.current.get() {
                        remaining += dl.total.get() - dl.current.get();
                    }
                }
                if remaining > 0 && dur > Duration::from_millis(500) {
                    msg.push_str(&format!(", remaining bytes: {:.1}", HumanBytes(remaining)));
                }
            }
        }
        progress.print_now(&msg)
    }
}

#[derive(Copy, Clone)]
enum WhyTick<'a> {
    DownloadStarted,
    DownloadUpdate,
    DownloadFinished,
    Extracting(&'a str),
}

impl<'a, 'gctx> Drop for Downloads<'a, 'gctx> {
    fn drop(&mut self) {
        self.set.downloading.set(false);
        let progress = self.progress.get_mut().take().unwrap();
        // Don't print a download summary if we're not using a progress bar,
        // we've already printed lots of `Downloading...` items.
        if !progress.is_enabled() {
            return;
        }
        // If we didn't download anything, no need for a summary.
        if self.downloads_finished == 0 {
            return;
        }
        // If an error happened, let's not clutter up the output.
        if !self.success {
            return;
        }
        // pick the correct plural of crate(s)
        let crate_string = if self.downloads_finished == 1 {
            "crate"
        } else {
            "crates"
        };
        let mut status = format!(
            "{} {} ({:.1}) in {}",
            self.downloads_finished,
            crate_string,
            HumanBytes(self.downloaded_bytes),
            util::elapsed(self.start.elapsed())
        );
        // print the size of largest crate if it was >1mb
        // however don't print if only a single crate was downloaded
        // because it is obvious that it will be the largest then
        let mib_1 = 1024 * 1024;
        if self.largest.0 > mib_1 && self.downloads_finished > 1 {
            status.push_str(&format!(
                " (largest was `{}` at {:.1})",
                self.largest.1,
                HumanBytes(self.largest.0),
            ));
        }
        // Clear progress before displaying final summary.
        drop(progress);
        drop(self.set.gctx.shell().status("Downloaded", status));
    }
}

mod tls {
    use std::cell::Cell;

    use super::Downloads;

    thread_local!(static PTR: Cell<usize> = const { Cell::new(0) });

    pub(crate) fn with<R>(f: impl FnOnce(Option<&Downloads<'_, '_>>) -> R) -> R {
        let ptr = PTR.with(|p| p.get());
        if ptr == 0 {
            f(None)
        } else {
            unsafe { f(Some(&*(ptr as *const Downloads<'_, '_>))) }
        }
    }

    pub(crate) fn set<R>(dl: &Downloads<'_, '_>, f: impl FnOnce() -> R) -> R {
        struct Reset<'a, T: Copy>(&'a Cell<T>, T);

        impl<'a, T: Copy> Drop for Reset<'a, T> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }

        PTR.with(|p| {
            let _reset = Reset(p, p.get());
            p.set(dl as *const Downloads<'_, '_> as usize);
            f()
        })
    }
}
