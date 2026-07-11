//! Utility functions for ia-get.

use crate::constants::URL_PATTERN;
use crate::{IaGetError, Result};
use colored::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fs;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Spinner tick interval in milliseconds
pub const SPINNER_TICK_INTERVAL: u64 = 100;

/// Size constants for formatting
const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;

/// Compiled regex for URL validation (initialized once)
static URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(URL_PATTERN).expect("Invalid URL regex pattern"));

/// Validates an archive.org details URL format
///
/// # Arguments
/// * `url` - The URL to validate
///
/// # Returns
/// * `Ok(())` if the URL is valid
/// * `Err(IaGetError::UrlFormat)` if the URL format is invalid
///
/// # Examples
/// ```
/// use ia_get::utils::validate_archive_url;
///
/// assert!(validate_archive_url("https://archive.org/details/valid-item").is_ok());
/// assert!(validate_archive_url("https://archive.org/details/valid-item/").is_ok());
/// assert!(validate_archive_url("https://example.com/invalid").is_err());
/// ```
pub fn validate_archive_url(url: &str) -> Result<()> {
    if URL_REGEX.is_match(url) {
        // Further check: ensure there's an identifier after "details/"
        // and that the identifier is not empty.
        if let Some(path_segment) = url.split("/details/").nth(1) {
            if !path_segment.trim_end_matches('/').is_empty() {
                return Ok(());
            }
        }
    }
    Err(IaGetError::UrlFormat(url.to_string()))
}

/// Create a progress bar with consistent styling
///
/// # Arguments
/// * `total` - Total value for the progress bar
/// * `action` - Action text to show at the beginning (e.g., "╰╼ Downloading  ")
/// * `color` - Optional color style (defaults to "green/green")
/// * `with_eta` - Whether to include ETA in the template
///
/// # Returns
/// A configured progress bar
pub fn create_progress_bar(
    total: u64,
    action: &str,
    color: Option<&str>,
    with_eta: bool,
) -> ProgressBar {
    create_progress_bar_in(total, action, color, with_eta, None)
}

/// Create a progress bar, optionally attached to a [`MultiProgress`] manager.
///
/// When `multi_progress` is set, the bar is drawn on its own line so concurrent
/// downloads do not overwrite each other.
pub fn create_progress_bar_in(
    total: u64,
    action: &str,
    color: Option<&str>,
    with_eta: bool,
    multi_progress: Option<&MultiProgress>,
) -> ProgressBar {
    let pb = ProgressBar::new(total);
    let color_str = color.unwrap_or("green/green");

    let styled_action = if action.contains("├╼") || action.contains("╰╼") {
        action
            .replace("├╼", &"├╼".cyan().dimmed().to_string())
            .replace("╰╼", &"╰╼".cyan().dimmed().to_string())
    } else {
        action.to_string()
    };

    let template = if with_eta {
        format!(
            "{}{{elapsed_precise}} {{bar:40.{}}} {{bytes}}/{{total_bytes}} {{wide_msg}}",
            styled_action, color_str
        )
    } else {
        format!(
            "{}{{elapsed_precise}} {{bar:40.{}}} {{bytes}}/{{total_bytes}}",
            styled_action, color_str
        )
    };

    pb.set_style(
        ProgressStyle::default_bar()
            .template(&template)
            .expect("Failed to set progress bar style")
            .progress_chars("▓▒░"),
    );

    match multi_progress {
        Some(manager) => manager.add(pb),
        None => pb,
    }
}

/// Truncate a filename label for compact progress-bar display in narrow terminals.
fn truncate_file_label(label: &str, max_chars: usize) -> String {
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label.to_string();
    }

    let trimmed: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

/// Create a progress bar for one slot in a parallel download view.
///
/// Each bar is inserted at a fixed index so concurrent downloads keep a stable,
/// ordered layout in Windows CMD and other terminals.
pub fn create_parallel_progress_bar(
    multi_progress: &MultiProgress,
    slot: usize,
    total: u64,
    action: &str,
    file_label: &str,
    color: Option<&str>,
    with_eta: bool,
) -> ProgressBar {
    let pb = ProgressBar::new(total);
    let color_str = color.unwrap_or("green/green");
    let display_label = truncate_file_label(file_label, 26);
    let prefix = format!(
        "{} {} {} ",
        "╰╼".cyan().dimmed(),
        action.white(),
        display_label.dimmed()
    );

    let template = if with_eta {
        format!(
            "{prefix}{{elapsed_precise}} {{bar:28.{color_str}}} {{bytes}}/{{total_bytes}} {{wide_msg}}"
        )
    } else {
        format!(
            "{prefix}{{elapsed_precise}} {{bar:28.{color_str}}} {{bytes}}/{{total_bytes}}"
        )
    };

    pb.set_style(
        ProgressStyle::default_bar()
            .template(&template)
            .expect("Failed to set parallel progress bar style")
            .progress_chars("▓▒░"),
    );

    multi_progress.insert(slot, pb)
}

/// Create a spinner with braille animation
///
/// # Arguments
/// * `message` - Message to display next to the spinner
///
/// # Returns
/// A configured spinner
pub fn create_spinner(message: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template(&format!("{} {}", "{spinner}".yellow().bold(), message))
            .expect("Failed to set spinner style"),
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(SPINNER_TICK_INTERVAL));
    spinner
}

/// Rolling download speed calculator (aria2 `SpeedCalc`, 10-second window).
#[derive(Debug, Clone)]
pub struct SpeedCalc {
    time_slots: VecDeque<(Instant, u64)>,
    bytes_window: u64,
    accumulated_length: u64,
}

const SPEED_WINDOW: Duration = Duration::from_secs(10);

impl SpeedCalc {
    pub fn new() -> Self {
        Self {
            time_slots: VecDeque::new(),
            bytes_window: 0,
            accumulated_length: 0,
        }
    }

    pub fn accumulated_length(&self) -> u64 {
        self.accumulated_length
    }

    pub fn update(&mut self, bytes: u64) {
        let now = Instant::now();
        self.remove_stale(now);

        if let Some((last_time, last_bytes)) = self.time_slots.back_mut() {
            if now.duration_since(*last_time) < Duration::from_secs(1) {
                *last_bytes += bytes;
            } else {
                self.time_slots.push_back((now, bytes));
            }
        } else {
            self.time_slots.push_back((now, bytes));
        }

        self.bytes_window += bytes;
        self.accumulated_length += bytes;
    }

    /// Current transfer speed in bytes per second (aria2 `calculateSpeed`).
    pub fn speed_bps(&mut self) -> u64 {
        let now = Instant::now();
        self.remove_stale(now);

        if self.time_slots.is_empty() {
            return 0;
        }

        let elapsed_ms = now
            .duration_since(self.time_slots.front().expect("slot").0)
            .as_millis()
            .max(1) as u64;

        self.bytes_window.saturating_mul(1000) / elapsed_ms
    }

    fn remove_stale(&mut self, now: Instant) {
        while let Some((time, _)) = self.time_slots.front() {
            if now.duration_since(*time) <= SPEED_WINDOW {
                break;
            }
            if let Some((_, bytes)) = self.time_slots.pop_front() {
                self.bytes_window = self.bytes_window.saturating_sub(bytes);
            }
        }
    }
}

/// ETA in seconds using aria2's formula: `(total - completed) / speed`.
pub fn eta_seconds(total: u64, completed: u64, speed_bps: u64) -> Option<u64> {
    if speed_bps == 0 || completed >= total {
        return None;
    }
    Some((total - completed).div_ceil(speed_bps))
}

pub fn format_progress_eta_message(total: u64, completed: u64, speed_bps: u64) -> String {
    eta_seconds(total, completed, speed_bps)
        .map(|secs| format!("(ETA: {})", format_duration(Duration::from_secs(secs))))
        .unwrap_or_default()
}

/// Update a progress bar using session transfer stats (resume-safe ETA).
pub fn update_transfer_progress_bar(
    pb: &ProgressBar,
    speed: &mut SpeedCalc,
    baseline_completed: u64,
    total: u64,
    session_chunk: u64,
) {
    speed.update(session_chunk);
    let completed = baseline_completed.saturating_add(speed.accumulated_length());
    pb.set_position(completed);
    pb.set_message(format_progress_eta_message(total, completed, speed.speed_bps()));
}

/// Format a duration into a human-readable string
pub fn format_duration(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs < 60 {
        return format!("{}.{:02}s", total_secs, duration.subsec_millis() / 10);
    }

    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {}m {}s", hours, mins, secs)
    } else {
        format!("{}m {}s", mins, secs)
    }
}

/// Format a size in bytes to a human-readable string
pub fn format_size(size: u64) -> String {
    if size < KB {
        format!("{}B", size)
    } else if size < MB {
        format!("{:.2}KB", size as f64 / KB as f64)
    } else if size < GB {
        format!("{:.2}MB", size as f64 / MB as f64)
    } else {
        format!("{:.2}GB", size as f64 / GB as f64)
    }
}

/// Format transfer rate to appropriate units
pub fn format_transfer_rate(bytes_per_sec: f64) -> (f64, &'static str) {
    let kb = KB as f64;
    let mb = MB as f64;
    let gb = GB as f64;

    if bytes_per_sec < kb {
        (bytes_per_sec, "B")
    } else if bytes_per_sec < mb {
        (bytes_per_sec / kb, "KB")
    } else if bytes_per_sec < gb {
        (bytes_per_sec / mb, "MB")
    } else {
        (bytes_per_sec / gb, "GB")
    }
}

/// Parsed include/exclude extension filters from the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionFilterSpec {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

/// Split CLI filter args into include (`*apk`) and exclude (`#jpg`) groups.
pub fn split_extension_filter_args(filters: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();

    for filter in filters {
        if filter.starts_with('*') {
            includes.push(filter.clone());
        } else if filter.starts_with('#') {
            excludes.push(filter.clone());
        } else {
            return Err(IaGetError::UrlFormat(format!(
                "Invalid filter '{filter}'. Use *ext to include or #ext to exclude, for example *apk or #jpg"
            )));
        }
    }

    Ok((includes, excludes))
}

/// Parse include and exclude filters from the CLI into normalized extensions.
pub fn parse_extension_filters(filters: &[String]) -> Result<ExtensionFilterSpec> {
    let (includes, excludes) = split_extension_filter_args(filters)?;
    Ok(ExtensionFilterSpec {
        includes: normalize_extension_filters(&includes)?,
        excludes: normalize_extension_excludes(&excludes)?,
    })
}

/// Convert CLI excludes like `#jpg` into normalized extensions like `.jpg`.
pub fn normalize_extension_excludes(excludes: &[String]) -> Result<Vec<String>> {
    excludes
        .iter()
        .map(|filter| {
            if !filter.starts_with('#') {
                return Err(IaGetError::UrlFormat(format!(
                    "Invalid exclude '{filter}'. Excludes must start with #, for example #jpg or #torrent"
                )));
            }
            let ext = filter.trim_start_matches('#').trim_start_matches('.');
            if ext.is_empty() {
                return Err(IaGetError::UrlFormat(
                    "Invalid exclude. Provide an extension after #, for example #jpg".to_string(),
                ));
            }
            Ok(format!(".{}", ext.to_ascii_lowercase()))
        })
        .collect()
}

/// Convert CLI filters like `*apk` into normalized extensions like `.apk`.
pub fn normalize_extension_filters(filters: &[String]) -> Result<Vec<String>> {
    filters
        .iter()
        .map(|filter| {
            if !filter.starts_with('*') {
                return Err(IaGetError::UrlFormat(format!(
                    "Invalid filter '{filter}'. Filters must start with *, for example *apk or *xapk"
                )));
            }
            let ext = filter.trim_start_matches('*').trim_start_matches('.');
            if ext.is_empty() {
                return Err(IaGetError::UrlFormat(
                    "Invalid filter. Provide an extension after *, for example *apk".to_string(),
                ));
            }
            Ok(format!(".{}", ext.to_ascii_lowercase()))
        })
        .collect()
}

/// Returns true when no filters are set, or when the filename ends with one of them.
pub fn file_matches_extension_filters(filename: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }

    let lower = filename.to_ascii_lowercase();
    filters.iter().any(|ext| lower.ends_with(ext))
}

/// Returns true when a file should be downloaded given include/exclude filters.
///
/// With no includes, all files are candidates. With no excludes, nothing is blocked.
pub fn file_passes_extension_filters(
    filename: &str,
    includes: &[String],
    excludes: &[String],
) -> bool {
    if !file_matches_extension_filters(filename, includes) {
        return false;
    }

    if excludes.is_empty() {
        return true;
    }

    let lower = filename.to_ascii_lowercase();
    !excludes.iter().any(|ext| lower.ends_with(ext))
}

/// Sanitizes a filename for cross-platform filesystem compatibility
///
/// Replaces characters that are invalid on Windows or Unix filesystems
/// with underscores, while preserving path separators.
///
/// Invalid characters replaced with underscores:
/// - Windows: `< > : " | ? *` and control characters (0-31)
/// - Unix: null character (\0)
/// - Both: leading/trailing spaces, trailing dots in path components
///
/// Also handles Windows reserved names (CON, PRN, AUX, NUL, COM1-9, LPT1-9)
/// by appending an underscore.
///
/// # Arguments
/// * `filename` - The original filename (may include path components separated by `/`)
///
/// # Returns
/// * `(sanitized_filename, was_modified)` - Tuple of cleaned filename and whether it was changed
///
/// # Examples
/// ```
/// use ia_get::utils::sanitize_filename;
///
/// let (sanitized, modified) = sanitize_filename("normal_file.txt");
/// assert_eq!(sanitized, "normal_file.txt");
/// assert!(!modified);
///
/// let (sanitized, modified) = sanitize_filename("file?name.txt");
/// assert_eq!(sanitized, "file_name.txt");
/// assert!(modified);
///
/// let (sanitized, modified) = sanitize_filename("Season 1/Episode?.mp4");
/// assert_eq!(sanitized, "Season 1/Episode_.mp4");
/// assert!(modified);
/// ```
pub fn sanitize_filename(filename: &str) -> (String, bool) {
    // Windows reserved names (case-insensitive)
    const RESERVED_NAMES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let mut was_modified = false;
    let mut result = String::with_capacity(filename.len());

    // Process each path component separately to preserve directory structure
    let components: Vec<&str> = filename.split('/').collect();
    let mut first_component = true;

    for component in components.iter() {
        // Skip empty components (e.g., from leading/trailing slashes or "//" sequences)
        if component.is_empty() {
            if !filename.is_empty() {
                was_modified = true;
            }
            continue;
        }

        // Add separator before non-first components
        if !first_component {
            result.push('/');
        }
        first_component = false;

        let mut sanitized_component = String::with_capacity(component.len());

        // Replace invalid characters
        for ch in component.chars() {
            match ch {
                // Windows invalid characters
                '<' | '>' | ':' | '"' | '|' | '?' | '*' => {
                    sanitized_component.push('_');
                    was_modified = true;
                }
                // Backslash (path separator on Windows, invalid in filenames on Unix)
                '\\' => {
                    sanitized_component.push('_');
                    was_modified = true;
                }
                // Control characters (0-31) and DEL (127)
                '\x00'..='\x1F' | '\x7F' => {
                    sanitized_component.push('_');
                    was_modified = true;
                }
                // Valid character
                _ => sanitized_component.push(ch),
            }
        }

        // Trim leading/trailing spaces
        let trimmed = sanitized_component.trim();
        if trimmed.len() != sanitized_component.len() {
            was_modified = true;
            sanitized_component = trimmed.to_string();
        }

        // Trim trailing dots (Windows doesn't allow filenames ending with dots)
        let trimmed_dots = sanitized_component.trim_end_matches('.');
        if trimmed_dots.len() != sanitized_component.len() {
            was_modified = true;
            sanitized_component = trimmed_dots.to_string();
        }

        // Handle empty components after sanitization
        if sanitized_component.is_empty() {
            sanitized_component = "_".to_string();
            was_modified = true;
        }

        // Check for Windows reserved names
        // Split by '.' to check the base name (before extension)
        let dot_pos = sanitized_component.find('.');
        let base_name = if let Some(pos) = dot_pos {
            &sanitized_component[..pos]
        } else {
            &sanitized_component
        };

        if RESERVED_NAMES
            .iter()
            .any(|&reserved| base_name.eq_ignore_ascii_case(reserved))
        {
            // Insert underscore after base name, before extension
            if let Some(pos) = dot_pos {
                sanitized_component.insert(pos, '_');
            } else {
                sanitized_component.push('_');
            }
            was_modified = true;
        }

        result.push_str(&sanitized_component);
    }

    // Remove trailing slash if present (unless it's just "/")
    if result.len() > 1 && result.ends_with('/') {
        result.pop();
        was_modified = true;
    }

    (result, was_modified)
}

/// Resolve the local download path for an archive file entry.
///
/// Without include/exclude filters, the full sanitized archive path is always used.
///
/// With filters and `keep_folder_structure` or `partial_folder_structure`, the full path is kept
/// (e.g. `apk/game.apk`).
/// With filters and without either flag, only the basename is used (e.g. `game.apk`).
pub fn resolve_local_download_path(
    archive_path: &str,
    keep_folder_structure: bool,
    partial_folder_structure: bool,
    has_filters: bool,
) -> (String, bool) {
    let (sanitized, mut was_modified) = sanitize_filename(archive_path);

    if !has_filters {
        return (sanitized, was_modified);
    }

    if keep_folder_structure || partial_folder_structure {
        return (sanitized, was_modified);
    }

    let flattened = sanitized
        .rsplit('/')
        .next()
        .filter(|component| !component.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| sanitized.clone());

    if flattened != sanitized {
        was_modified = true;
    }

    (flattened, was_modified)
}

/// Collect every directory path implied by archive file paths.
///
/// Parent folders of files are included. Folders that exist in the archive layout
/// but contain no files in `_files.xml` cannot be listed explicitly by archive.org;
/// this returns all directory prefixes present in the metadata file list.
pub fn collect_archive_directories(archive_paths: &[String]) -> Vec<String> {
    let mut dirs = BTreeSet::new();

    for archive_path in archive_paths {
        let (sanitized, _) = sanitize_filename(archive_path);
        let parts: Vec<&str> = sanitized.split('/').filter(|part| !part.is_empty()).collect();
        if parts.len() <= 1 {
            continue;
        }
        for i in 1..parts.len() {
            dirs.insert(parts[..i].join("/"));
        }
    }

    dirs.into_iter().collect()
}

/// Create archive directory paths locally (including folders that end up empty).
pub fn create_archive_directories(directories: &[String]) -> Result<()> {
    for dir in directories {
        fs::create_dir_all(dir).map_err(|e| {
            IaGetError::FileSystem(format!("Failed to create directory '{dir}': {e}"))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_valid_filename() {
        let (result, modified) = sanitize_filename("normal_file-name.txt");
        assert_eq!(result, "normal_file-name.txt");
        assert!(!modified);
    }

    #[test]
    fn test_sanitize_valid_filename_with_path() {
        let (result, modified) = sanitize_filename("folder/subfolder/file.txt");
        assert_eq!(result, "folder/subfolder/file.txt");
        assert!(!modified);
    }

    #[test]
    fn test_sanitize_invalid_characters() {
        let (result, modified) = sanitize_filename("file?name:test<>.txt");
        assert_eq!(result, "file_name_test__.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_question_mark() {
        let (result, modified) = sanitize_filename("Episode?.mp4");
        assert_eq!(result, "Episode_.mp4");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_with_path() {
        let (result, modified) = sanitize_filename("Season 1/Episode?.mp4");
        assert_eq!(result, "Season 1/Episode_.mp4");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_multiple_invalid_in_path() {
        let (result, modified) = sanitize_filename("Folder:Name/File*Name?.txt");
        assert_eq!(result, "Folder_Name/File_Name_.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_windows_reserved_names() {
        let (result, modified) = sanitize_filename("CON.txt");
        assert_eq!(result, "CON_.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("con.txt");
        assert_eq!(result, "con_.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("PRN");
        assert_eq!(result, "PRN_");
        assert!(modified);

        let (result, modified) = sanitize_filename("aux.log");
        assert_eq!(result, "aux_.log");
        assert!(modified);

        let (result, modified) = sanitize_filename("COM1.dat");
        assert_eq!(result, "COM1_.dat");
        assert!(modified);

        let (result, modified) = sanitize_filename("LPT9.txt");
        assert_eq!(result, "LPT9_.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_reserved_in_path() {
        let (result, modified) = sanitize_filename("folder/CON.txt");
        assert_eq!(result, "folder/CON_.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_control_characters() {
        let (result, modified) = sanitize_filename("file\x00\x1fname.txt");
        assert_eq!(result, "file__name.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("test\x7Ffile.txt");
        assert_eq!(result, "test_file.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_backslash() {
        let (result, modified) = sanitize_filename("folder\\file.txt");
        assert_eq!(result, "folder_file.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_whitespace_edge_cases() {
        let (result, modified) = sanitize_filename(" leading.txt ");
        assert_eq!(result, "leading.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("folder/ spaces /file.txt");
        assert_eq!(result, "folder/spaces/file.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_trailing_dots() {
        let (result, modified) = sanitize_filename("file...");
        assert_eq!(result, "file");
        assert!(modified);

        let (result, modified) = sanitize_filename("folder./file.txt");
        assert_eq!(result, "folder/file.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_empty_components() {
        let (result, modified) = sanitize_filename("folder//file.txt");
        assert_eq!(result, "folder/file.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("/folder/file.txt");
        assert_eq!(result, "folder/file.txt");
        assert!(modified);

        let (result, modified) = sanitize_filename("folder/file.txt/");
        assert_eq!(result, "folder/file.txt");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_all_invalid() {
        let (result, modified) = sanitize_filename("???");
        assert_eq!(result, "___");
        assert!(modified);
    }

    #[test]
    fn test_sanitize_unicode() {
        let (result, modified) = sanitize_filename("файл.txt");
        assert_eq!(result, "файл.txt");
        assert!(!modified);

        let (result, modified) = sanitize_filename("文件.txt");
        assert_eq!(result, "文件.txt");
        assert!(!modified);

        let (result, modified) = sanitize_filename("emoji😀.txt");
        assert_eq!(result, "emoji😀.txt");
        assert!(!modified);
    }

    #[test]
    fn test_sanitize_mixed_valid_invalid() {
        let (result, modified) =
            sanitize_filename("Red vs. Blue - Season 1/Episode 1: Why Are We Here?.mp4");
        assert_eq!(
            result,
            "Red vs. Blue - Season 1/Episode 1_ Why Are We Here_.mp4"
        );
        assert!(modified);
    }

    #[test]
    fn test_sanitize_preserves_extension() {
        let (result, modified) = sanitize_filename("file:name.tar.gz");
        assert_eq!(result, "file_name.tar.gz");
        assert!(modified);
    }

    #[test]
    fn extension_filters_match_case_insensitive() {
        let filters =
            normalize_extension_filters(&["*APK".to_string(), "*XAPK".to_string()]).unwrap();
        assert!(file_matches_extension_filters("apps/game.APK", &filters));
        assert!(file_matches_extension_filters("bundle/file.xapk", &filters));
        assert!(!file_matches_extension_filters("readme.txt", &filters));
    }

    #[test]
    fn empty_filters_match_everything() {
        let filters = normalize_extension_filters(&[]).unwrap();
        assert!(file_matches_extension_filters("anything.bin", &filters));
    }

    #[test]
    fn invalid_filter_must_start_with_star() {
        assert!(normalize_extension_filters(&["apk".to_string()]).is_err());
    }

    #[test]
    fn extension_excludes_match_case_insensitive() {
        let excludes = normalize_extension_excludes(&["#JPG".to_string(), "#torrent".to_string()])
            .unwrap();
        assert!(!file_passes_extension_filters("photo.JPG", &[], &excludes));
        assert!(!file_passes_extension_filters("data/file.torrent", &[], &excludes));
        assert!(file_passes_extension_filters("readme.txt", &[], &excludes));
    }

    #[test]
    fn empty_excludes_match_everything() {
        let spec = parse_extension_filters(&[]).unwrap();
        assert!(file_passes_extension_filters(
            "anything.bin",
            &spec.includes,
            &spec.excludes
        ));
    }

    #[test]
    fn parse_extension_filters_splits_include_and_exclude() {
        let spec =
            parse_extension_filters(&["*apk".to_string(), "#jpg".to_string(), "#torrent".to_string()])
                .unwrap();
        assert_eq!(spec.includes, vec![".apk"]);
        assert_eq!(spec.excludes, vec![".jpg", ".torrent"]);
        assert!(file_passes_extension_filters("game.apk", &spec.includes, &spec.excludes));
        assert!(!file_passes_extension_filters("cover.jpg", &spec.includes, &spec.excludes));
        assert!(!file_passes_extension_filters("photo.jpg", &[], &spec.excludes));
    }

    #[test]
    fn invalid_filter_must_use_star_or_hash_prefix() {
        assert!(split_extension_filter_args(&["jpg".to_string()]).is_err());
    }

    #[test]
    fn resolve_local_download_path_keeps_structure_when_requested() {
        let (path, modified) = resolve_local_download_path("apk/game.apk", true, false, true);
        assert_eq!(path, "apk/game.apk");
        assert!(!modified);
    }

    #[test]
    fn resolve_local_download_path_keeps_structure_for_partial_folder_flag() {
        let (path, modified) = resolve_local_download_path("apk/game.apk", false, true, true);
        assert_eq!(path, "apk/game.apk");
        assert!(!modified);
    }

    #[test]
    fn resolve_local_download_path_flattens_filtered_downloads_by_default() {
        let (path, modified) = resolve_local_download_path("apk/game.apk", false, false, true);
        assert_eq!(path, "game.apk");
        assert!(modified);
    }

    #[test]
    fn resolve_local_download_path_keeps_structure_without_filters() {
        let (path, modified) = resolve_local_download_path("apk/game.apk", false, false, false);
        assert_eq!(path, "apk/game.apk");
        assert!(!modified);
    }

    #[test]
    fn resolve_local_download_path_applies_sanitization_before_flattening() {
        let (path, modified) = resolve_local_download_path("apk/game?.apk", false, false, true);
        assert_eq!(path, "game_.apk");
        assert!(modified);
    }

    #[test]
    fn collect_archive_directories_includes_nested_and_empty_parents() {
        let paths = vec![
            "apk/game.apk".to_string(),
            "data/readme.txt".to_string(),
            "deep/nested/file.zip".to_string(),
        ];
        assert_eq!(
            collect_archive_directories(&paths),
            vec![
                "apk".to_string(),
                "data".to_string(),
                "deep".to_string(),
                "deep/nested".to_string(),
            ]
        );
    }

    #[test]
    fn eta_seconds_matches_aria2_remaining_over_speed() {
        assert_eq!(eta_seconds(1_000, 100, 50), Some(18));
        assert_eq!(eta_seconds(1_000, 1_000, 50), None);
        assert_eq!(eta_seconds(1_000, 100, 0), None);
    }

    #[test]
    fn speed_calc_reports_session_bytes_only() {
        let mut speed = SpeedCalc::new();
        speed.update(512);
        speed.update(512);
        assert_eq!(speed.accumulated_length(), 1024);
    }

    #[test]
    fn collect_archive_directories_ignores_root_level_files() {
        let paths = vec!["readme.txt".to_string(), "cover.jpg".to_string()];
        assert!(collect_archive_directories(&paths).is_empty());
    }
}
