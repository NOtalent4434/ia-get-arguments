//! Module for handling file downloads, verification, and related operations.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use colored::*;
use indicatif::{MultiProgress, ProgressBar};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::error::IaGetError;
use crate::utils::{
    create_parallel_progress_bar, create_progress_bar_in, format_duration, format_size,
    format_transfer_rate, update_transfer_progress_bar, SpeedCalc,
};
use crate::Result;

/// Buffer size for file operations (8KB)
const BUFFER_SIZE: usize = 8192;

/// File size threshold for showing hash progress bar (2MB)
const LARGE_FILE_THRESHOLD: u64 = 2 * 1024 * 1024;

/// Maximum number of retry attempts for failed downloads
const MAX_RETRIES: u32 = 3;

/// Initial delay between retries in milliseconds (doubles with each retry)
const INITIAL_RETRY_DELAY_MS: u64 = 1000;

/// Download behaviour controlled by `ia-get.ini`.
#[derive(Debug, Clone, Copy)]
pub struct DownloadOptions {
    pub max_bytes_per_sec: Option<u64>,
    pub multithreading: bool,
    pub thread_count: u32,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            max_bytes_per_sec: None,
            multithreading: false,
            thread_count: 4,
        }
    }
}

struct BandwidthLimiter {
    max_bytes_per_sec: u64,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    /// Available byte budget (token bucket), capped to one second of throughput.
    tokens: f64,
    last_refill: Instant,
}

impl BandwidthLimiter {
    fn new(max_bytes_per_sec: u64) -> Self {
        Self {
            max_bytes_per_sec,
            state: Mutex::new(LimiterState {
                tokens: max_bytes_per_sec as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Wait until `bytes` may be transferred without exceeding the global cap.
    ///
    /// All concurrent downloads share one limiter, matching aria2's
    /// `max-overall-download-limit` behaviour: total throughput is capped and
    /// active downloads compete fairly for the same budget.
    async fn acquire(&self, bytes: u64) {
        let bytes = bytes.max(1) as f64;
        let max_rate = self.max_bytes_per_sec as f64;

        loop {
            let sleep_for = {
                let mut guard = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(guard.last_refill).as_secs_f64();
                if elapsed > 0.0 {
                    guard.tokens = (guard.tokens + elapsed * max_rate).min(max_rate);
                    guard.last_refill = now;
                }

                if guard.tokens >= bytes {
                    guard.tokens -= bytes;
                    None
                } else {
                    let deficit = bytes - guard.tokens;
                    Some(Duration::from_secs_f64(deficit / max_rate))
                }
            };

            if let Some(delay) = sleep_for {
                if delay > Duration::from_millis(1) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }

            break;
        }
    }
}

fn concurrent_download_slots(options: DownloadOptions) -> u32 {
    if options.multithreading {
        options.thread_count.max(1)
    } else {
        1
    }
}

/// Coordinates progress bars and log output for sequential or parallel downloads.
#[derive(Clone)]
struct DownloadDisplay {
    output_lock: Arc<StdMutex<()>>,
}

impl DownloadDisplay {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            output_lock: Arc::new(StdMutex::new(())),
        })
    }

    fn print_immediate(&self, message: impl Into<String>) {
        let _guard = self.output_lock.lock().expect("output lock");
        println!("{}", message.into());
    }

    fn print(&self, message: impl Into<String>) {
        self.print_immediate(message);
    }
}

/// Serialises per-file console output in index order while downloads run in parallel.
struct OrderedParallelDisplay {
    display: Arc<DownloadDisplay>,
    multi_progress: Arc<MultiProgress>,
    state: Arc<StdMutex<OrderedDisplayState>>,
}

struct OrderedDisplayState {
    next: usize,
    files: BTreeMap<usize, OrderedFileSlot>,
}

struct OrderedFileSlot {
    lines: Vec<String>,
    emitted: usize,
    finished: bool,
}

impl OrderedParallelDisplay {
    fn new(display: Arc<DownloadDisplay>) -> Arc<Self> {
        Arc::new(Self {
            display,
            multi_progress: Arc::new(MultiProgress::new()),
            state: Arc::new(StdMutex::new(OrderedDisplayState {
                next: 0,
                files: BTreeMap::new(),
            })),
        })
    }

    fn multi_progress(&self) -> &MultiProgress {
        self.multi_progress.as_ref()
    }

    fn print_line(&self, line: String) {
        self.multi_progress.suspend(|| {
            self.display.print_immediate(line);
        });
    }

    fn push_immediate(&self, line: String) {
        self.print_line(line);
    }

    fn push_line(&self, index: usize, line: String) {
        let mut state = self.state.lock().expect("ordered display lock");
        state
            .files
            .entry(index)
            .or_insert_with(|| OrderedFileSlot {
                lines: Vec::new(),
                emitted: 0,
                finished: false,
            })
            .lines
            .push(line);
        self.flush_ready_locked(&mut state);
    }

    fn finish_file(&self, index: usize) {
        let mut state = self.state.lock().expect("ordered display lock");
        state
            .files
            .entry(index)
            .or_insert_with(|| OrderedFileSlot {
                lines: Vec::new(),
                emitted: 0,
                finished: false,
            })
            .finished = true;
        self.flush_ready_locked(&mut state);
    }

    fn flush_ready_locked(&self, state: &mut OrderedDisplayState) {
        loop {
            let Some(slot) = state.files.get_mut(&state.next) else {
                break;
            };

            while slot.emitted < slot.lines.len() {
                let line = slot.lines[slot.emitted].clone();
                slot.emitted += 1;
                self.print_line(line);
            }

            if !slot.finished {
                break;
            }

            state.files.remove(&state.next);
            state.next += 1;
        }
    }
}

enum DownloadUiMode<'a> {
    Sequential,
    Ordered {
        index: usize,
        file_label: &'a str,
        coordinator: &'a OrderedParallelDisplay,
    },
}

/// Per-file console output — live in sequential mode, ordered streaming in parallel mode.
struct DownloadUi<'a> {
    display: &'a DownloadDisplay,
    mode: DownloadUiMode<'a>,
}

impl<'a> DownloadUi<'a> {
    fn sequential(display: &'a DownloadDisplay) -> Self {
        Self {
            display,
            mode: DownloadUiMode::Sequential,
        }
    }

    fn ordered(
        index: usize,
        file_label: &'a str,
        coordinator: &'a OrderedParallelDisplay,
        display: &'a DownloadDisplay,
    ) -> Self {
        Self {
            display,
            mode: DownloadUiMode::Ordered {
                index,
                file_label,
                coordinator,
            },
        }
    }

    fn print(&mut self, message: impl Into<String>) {
        let message = message.into();
        match self.mode {
            DownloadUiMode::Sequential => self.display.print_immediate(message),
            DownloadUiMode::Ordered { index, coordinator, .. } => {
                coordinator.push_line(index, message)
            }
        }
    }

    /// Status lines shown as soon as they are known (partial, retries, already complete).
    fn print_status(&mut self, message: impl Into<String>) {
        let message = message.into();
        match self.mode {
            DownloadUiMode::Sequential => self.display.print_immediate(message),
            DownloadUiMode::Ordered { coordinator, .. } => coordinator.push_immediate(message),
        }
    }

    fn progress_bar(
        &mut self,
        total: u64,
        action: &str,
        color: Option<&str>,
        with_eta: bool,
    ) -> ProgressBar {
        match self.mode {
            DownloadUiMode::Sequential => create_progress_bar_in(
                total,
                &progress_action_prefix(action),
                color,
                with_eta,
                None,
            ),
            DownloadUiMode::Ordered {
                index,
                file_label,
                coordinator,
            } => create_parallel_progress_bar(
                coordinator.multi_progress(),
                index,
                total,
                action,
                file_label,
                color,
                with_eta,
            ),
        }
    }
}

fn file_display_label(file_path: &str) -> String {
    Path::new(file_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string())
}

fn progress_action_prefix(action: &str) -> String {
    format!("{} {}  ", "╰╼".cyan().dimmed(), action.white())
}

fn format_file_header_lines(index: usize, total_files: usize, file_path: &str) -> [String; 3] {
    [
        " ".to_string(),
        format!(
            "{}  {}     {}",
            "▣".bright_cyan().bold(),
            "Filename".white(),
            file_path.bold()
        ),
        format!(
            "{} {}        {} {} of {}",
            "├╼".cyan().dimmed(),
            "Count".white(),
            "#".blue().bold(),
            (index + 1).to_string().bold(),
            total_files.to_string().bold()
        ),
    ]
}

fn print_all_parallel_file_headers(
    ordered: &OrderedParallelDisplay,
    file_list: &[(String, String, Option<String>)],
    total_files: usize,
) {
    for (index, (_, file_path, _)) in file_list.iter().enumerate() {
        for line in format_file_header_lines(index, total_files, file_path) {
            ordered.print_line(line);
        }
    }
}

/// Sets up signal handling for graceful shutdown on Ctrl+C
fn setup_signal_handler() -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
        println!(
            "\n{} Received Ctrl+C, finishing current operation...",
            "✘".red().bold()
        );
    })
    .expect("Error setting Ctrl+C handler");

    running
}

/// Calculates the MD5 hash of a file
fn calculate_md5(
    file_path: &str,
    running: &Arc<AtomicBool>,
    ui: &mut DownloadUi<'_>,
) -> Result<String> {
    let file = File::open(file_path)?;
    let file_size = file.metadata()?.len();
    let is_large_file = file_size > LARGE_FILE_THRESHOLD;

    let mut reader = BufReader::with_capacity(BUFFER_SIZE, file);
    let mut context = md5::Context::new();
    let mut buffer = [0; BUFFER_SIZE];

    let pb = if is_large_file {
        Some(ui.progress_bar(
            file_size,
            "Verifying",
            Some("blue/blue"),
            false,
        ))
    } else {
        None
    };

    let mut bytes_processed: u64 = 0;

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        if !running.load(Ordering::SeqCst) {
            if let Some(ref progress_bar) = pb {
                progress_bar.finish_and_clear();
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Hash calculation interrupted by signal",
            )
            .into());
        }

        context.consume(&buffer[..bytes_read]);

        if let Some(ref progress_bar) = pb {
            bytes_processed += bytes_read as u64;
            progress_bar.set_position(bytes_processed);
        }
    }

    if let Some(progress_bar) = pb.as_ref() {
        progress_bar.finish_and_clear();
    }

    let hash = context.finalize();
    Ok(format!("{:x}", hash))
}

fn check_existing_file(
    file_path: &str,
    expected_md5: Option<&str>,
    running: &Arc<AtomicBool>,
    ui: &mut DownloadUi<'_>,
) -> Result<Option<bool>> {
    if !Path::new(file_path).exists() {
        return Ok(None);
    }

    if expected_md5.is_none() {
        return Ok(Some(true));
    }

    let local_md5 = match calculate_md5(file_path, running, ui) {
        Ok(hash) => hash,
        Err(e) => {
            if e.to_string().contains("interrupted by signal") {
                return Err(e);
            }
            ui.print_status(format!(
                "{} {} to calculate MD5 hash: {}",
                "╰╼".cyan().dimmed(),
                "Failed".red().bold(),
                e
            ));
            return Ok(Some(false));
        }
    };

    Ok(Some(local_md5 == expected_md5.unwrap()))
}

fn ensure_parent_directories(file_path: &str) -> Result<()> {
    if let Some(path) = Path::new(file_path).parent() {
        if path.file_name().is_some() && !path.exists() {
            fs::create_dir_all(path)?;
        }
    }
    Ok(())
}

fn prepare_file_for_download(file_path: &str) -> Result<File> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(file_path)?;

    file.seek(SeekFrom::End(0))?;
    Ok(file)
}

async fn download_file_content(
    client: &Client,
    url: &str,
    file: &mut File,
    running: &Arc<AtomicBool>,
    cookie_header: Option<&str>,
    limiter: Option<&Arc<BandwidthLimiter>>,
    ui: &mut DownloadUi<'_>,
) -> Result<u64> {
    let mut retry_count = 0;

    loop {
        let current_file_size = file.metadata()?.len();
        let download_action = if current_file_size > 0 {
            "Resuming"
        } else {
            "Downloading"
        };

        let mut headers = HeaderMap::new();
        if let Some(cookie_header) = cookie_header {
            headers.insert(
                reqwest::header::COOKIE,
                HeaderValue::from_str(cookie_header).map_err(|e| {
                    IaGetError::Network(format!("Invalid cookie header value: {}", e))
                })?,
            );
        }
        if current_file_size > 0 {
            headers.insert(
                reqwest::header::RANGE,
                HeaderValue::from_str(&format!("bytes={current_file_size}-")).map_err(|e| {
                    IaGetError::Network(format!("Invalid range header value: {}", e))
                })?,
            );
        }

        let mut request = client.get(url);
        if !headers.is_empty() {
            request = request.headers(headers);
        }

        let mut response = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                retry_count += 1;

                if retry_count > MAX_RETRIES {
                    ui.print_status(format!(
                        "{} {}      {} Maximum retries ({}) exceeded",
                        "├╼".cyan().dimmed(),
                        "Failed".red().bold(),
                        "✘".red().bold(),
                        MAX_RETRIES
                    ));
                    return Err(e.into());
                }

                let delay = INITIAL_RETRY_DELAY_MS * 2u64.pow(retry_count - 1);
                ui.print_status(format!(
                    "{} {}      {} Connection error (attempt {}/{}): {}",
                    "├╼".cyan().dimmed(),
                    "Retry".yellow().bold(),
                    "⟳".yellow().bold(),
                    retry_count,
                    MAX_RETRIES,
                    e
                ));
                ui.print_status(format!(
                    "{} {}      Waiting {:.1}s before retry...",
                    "├╼".cyan().dimmed(),
                    "Wait".white(),
                    delay as f64 / 1000.0
                ));

                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                file.flush()?;
                file.seek(SeekFrom::End(0))?;
                continue;
            }
        };

        let content_length = response.content_length().unwrap_or(0);
        let total_expected_size = if current_file_size > 0 {
            content_length + current_file_size
        } else {
            content_length
        };

        let pb = ui.progress_bar(
            total_expected_size,
            download_action,
            Some("green/green"),
            true,
        );
        pb.set_position(current_file_size);
        pb.set_message(String::new());

        let start_time = std::time::Instant::now();
        let mut total_bytes: u64 = current_file_size;
        let mut downloaded_bytes: u64 = 0;
        let mut speed = SpeedCalc::new();

        let download_result: Result<()> = async {
            while let Some(chunk_result) = response.chunk().await.transpose() {
                if !running.load(Ordering::SeqCst) {
                    pb.finish_and_clear();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Download interrupted during file transfer",
                    )
                    .into());
                }

                let chunk = chunk_result?;
                if let Some(limiter) = limiter {
                    limiter.acquire(chunk.len() as u64).await;
                }
                file.write_all(&chunk)?;
                let chunk_len = chunk.len() as u64;
                downloaded_bytes += chunk_len;
                total_bytes += chunk_len;
                update_transfer_progress_bar(
                    &pb,
                    &mut speed,
                    current_file_size,
                    total_expected_size,
                    chunk_len,
                );
            }
            Ok(())
        }
        .await;

        match download_result {
            Ok(_) => {
                file.flush()?;

                let elapsed = start_time.elapsed();
                let elapsed_secs = elapsed.as_secs_f64();
                let transfer_rate_val = if elapsed_secs > 0.0 {
                    downloaded_bytes as f64 / elapsed_secs
                } else {
                    0.0
                };

                let (rate, unit) = format_transfer_rate(transfer_rate_val);

                pb.finish_and_clear();
                ui.print(format!(
                    "{} {}   {} {} in {} ({:.2} {}/s)",
                    "├╼".cyan().dimmed(),
                    "Downloaded".white(),
                    "↓".green().bold(),
                    format_size(downloaded_bytes).bold(),
                    format_duration(elapsed).bold(),
                    rate,
                    unit
                ));

                return Ok(total_bytes);
            }
            Err(e) => {
                pb.finish_and_clear();

                if e.to_string().contains("interrupted") {
                    return Err(e);
                }

                retry_count += 1;

                if retry_count > MAX_RETRIES {
                    ui.print_status(format!(
                        "{} {}      {} Maximum retries ({}) exceeded",
                        "├╼".cyan().dimmed(),
                        "Failed".red().bold(),
                        "✘".red().bold(),
                        MAX_RETRIES
                    ));
                    return Err(e);
                }

                let delay = INITIAL_RETRY_DELAY_MS * 2u64.pow(retry_count - 1);
                ui.print_status(format!(
                    "{} {}      {} Download error (attempt {}/{}): {}",
                    "├╼".cyan().dimmed(),
                    "Retry".yellow().bold(),
                    "⟳".yellow().bold(),
                    retry_count,
                    MAX_RETRIES,
                    e
                ));
                ui.print_status(format!(
                    "{} {}      Waiting {:.1}s before retry...",
                    "├╼".cyan().dimmed(),
                    "Wait".white(),
                    delay as f64 / 1000.0
                ));

                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                file.flush()?;
                file.seek(SeekFrom::End(0))?;
            }
        }
    }
}

fn verify_downloaded_file(
    file_path: &str,
    expected_md5: Option<&str>,
    running: &Arc<AtomicBool>,
    ui: &mut DownloadUi<'_>,
) -> Result<bool> {
    if expected_md5.is_none() {
        ui.print(format!(
            "{} {}",
            "-".dimmed(),
            "No MD5 hash provided for verification.".dimmed()
        ));
        return Ok(true);
    }
    let expected_md5_str = expected_md5.unwrap();
    let local_md5 = calculate_md5(file_path, running, ui)?;
    if local_md5 == expected_md5_str {
        ui.print(format!(
            "{} {}         {} {}",
            "╰╼".cyan().dimmed(),
            "Hash".white(),
            "✔".green().bold(),
            format!("({local_md5})").dimmed()
        ));
        Ok(true)
    } else {
        ui.print(format!(
            "{} {}         {} ({}) Expected ({})",
            "╰╼".cyan().dimmed(),
            "Hash".white(),
            "✘".red().bold(),
            local_md5.red(),
            expected_md5_str.dimmed()
        ));
        Ok(false)
    }
}

async fn download_single_file(
    client: &Client,
    index: usize,
    total_files: usize,
    url: String,
    file_path: String,
    expected_md5: Option<String>,
    cookie_header: Option<String>,
    running: Arc<AtomicBool>,
    limiter: Option<Arc<BandwidthLimiter>>,
    display: Arc<DownloadDisplay>,
    ordered: Option<Arc<OrderedParallelDisplay>>,
) -> Result<()> {
    let file_label = file_display_label(&file_path);
    let is_parallel = ordered.is_some();
    let mut ui = match ordered.as_ref() {
        Some(coordinator) => DownloadUi::ordered(
            index,
            &file_label,
            coordinator.as_ref(),
            display.as_ref(),
        ),
        None => DownloadUi::sequential(display.as_ref()),
    };

    if !is_parallel {
        for line in format_file_header_lines(index, total_files, &file_path) {
            ui.print(line);
        }
    }

    let result = async {
        if let Some(is_valid) =
            check_existing_file(&file_path, expected_md5.as_deref(), &running, &mut ui)?
        {
            if is_valid {
                ui.print_status(format!(
                    "{} {}   {}",
                    "╰╼".cyan().dimmed(),
                    "Downloaded".white(),
                    "✔".green().bold()
                ));
                return Ok(());
            }

            ui.print_status(format!(
                "{} {}      {}",
                "├╼".cyan().dimmed(),
                "Partial".white(),
                "▲".yellow().bold()
            ));
        }

        ensure_parent_directories(&file_path)?;
        let mut file = prepare_file_for_download(&file_path)?;

        download_file_content(
            client,
            &url,
            &mut file,
            &running,
            cookie_header.as_deref(),
            limiter.as_ref(),
            &mut ui,
        )
        .await?;
        verify_downloaded_file(&file_path, expected_md5.as_deref(), &running, &mut ui)?;
        Ok(())
    }
    .await;

    if let Some(coordinator) = ordered {
        coordinator.finish_file(index);
    }

    result
}

/// Download multiple files with shared signal handling.
pub async fn download_files<I>(
    client: &Client,
    files: I,
    total_files: usize,
    cookie_header: Option<&str>,
    options: DownloadOptions,
) -> Result<()>
where
    I: IntoIterator<Item = (String, String, Option<String>)>,
{
    let running = setup_signal_handler();
    let cookie_header = cookie_header.map(str::to_string);
    let file_list: Vec<_> = files.into_iter().collect();
    let slots = concurrent_download_slots(options);
    let shared_limiter = options
        .max_bytes_per_sec
        .map(BandwidthLimiter::new)
        .map(Arc::new);
    let display = DownloadDisplay::new();

    if options.multithreading {
        let semaphore = Arc::new(Semaphore::new(slots as usize));
        let ordered = OrderedParallelDisplay::new(display.clone());
        print_all_parallel_file_headers(&ordered, &file_list, total_files);
        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for (index, (url, file_path, expected_md5)) in file_list.into_iter().enumerate() {
            let client = client.clone();
            let running = running.clone();
            let cookie_header = cookie_header.clone();
            let semaphore = semaphore.clone();
            let limiter = shared_limiter.clone();
            let display = display.clone();
            let ordered = ordered.clone();

            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(|_| {
                    IaGetError::Network("Failed to acquire download slot".to_string())
                })?;

                if !running.load(Ordering::SeqCst) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Download interrupted before start",
                    )
                    .into());
                }

                download_single_file(
                    &client,
                    index,
                    total_files,
                    url,
                    file_path,
                    expected_md5,
                    cookie_header,
                    running,
                    limiter,
                    display,
                    Some(ordered),
                )
                .await
            });
        }

        let mut first_error = None;
        let mut interrupted = false;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if e.to_string().contains("interrupted") {
                        interrupted = true;
                        running.store(false, Ordering::SeqCst);
                        tasks.abort_all();
                    } else if first_error.is_none() {
                        first_error = Some(e);
                        running.store(false, Ordering::SeqCst);
                        tasks.abort_all();
                    }
                }
                Err(join_err) => {
                    if first_error.is_none() {
                        first_error = Some(IaGetError::Network(format!(
                            "Download task failed: {join_err}"
                        )));
                        running.store(false, Ordering::SeqCst);
                        tasks.abort_all();
                    }
                }
            }
        }

        if interrupted {
            display.print(format!(
                "\n{} Download interrupted. Run the command again to resume remaining files.",
                "✘".red().bold()
            ));
            return Ok(());
        }

        if let Some(e) = first_error {
            return Err(e);
        }
    } else {
        let limiter = shared_limiter;

        for (index, (url, file_path, expected_md5)) in file_list.into_iter().enumerate() {
            if !running.load(Ordering::SeqCst) {
                display.print(format!(
                    "\n{} Download interrupted. Run the command again to resume remaining files.",
                    "✘".red().bold()
                ));
                break;
            }

            download_single_file(
                client,
                index,
                total_files,
                url,
                file_path,
                expected_md5,
                cookie_header.clone(),
                running.clone(),
                limiter.clone(),
                display.clone(),
                None,
            )
            .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_download_slots_respects_multithreading_flag() {
        assert_eq!(
            concurrent_download_slots(DownloadOptions {
                multithreading: true,
                thread_count: 3,
                ..Default::default()
            }),
            3
        );
        assert_eq!(
            concurrent_download_slots(DownloadOptions {
                multithreading: false,
                thread_count: 8,
                ..Default::default()
            }),
            1
        );
    }

    #[test]
    fn ordered_parallel_display_flushes_in_index_order() {
        let display = DownloadDisplay::new();
        let ordered = OrderedParallelDisplay::new(display);

        ordered.push_line(1, "second".to_string());
        ordered.push_line(0, "first-a".to_string());
        ordered.push_line(0, "first-b".to_string());
        ordered.finish_file(0);
        ordered.push_line(1, "second-b".to_string());
        ordered.finish_file(1);

        let state = ordered.state.lock().expect("ordered display lock");
        assert_eq!(state.next, 2);
        assert!(state.files.is_empty());
    }
}
