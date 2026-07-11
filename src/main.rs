//! # ia-get
//!
//! A command-line tool for downloading files from the Internet Archive.
//!
//! This tool takes an archive.org details URL and downloads all associated files,
//! with support for resumable downloads and MD5 hash verification.

use clap::Parser;
use colored::*;
use ia_get::archive_metadata::{parse_xml_files, XmlFiles};
use ia_get::config::{self, Config};
use ia_get::constants::USER_AGENT;
use ia_get::downloader::{self, DownloadOptions};
use ia_get::utils::{
    create_spinner, file_passes_extension_filters, format_size, parse_extension_filters,
    collect_archive_directories, create_archive_directories, resolve_local_download_path,
    validate_archive_url,
};
use ia_get::Result;
use ia_get::IaGetError;
use indicatif::ProgressStyle;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE};
use reqwest::{Client, Url};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Extended timeout for large file downloads (10 minutes for connection, no read timeout)
const CONNECTION_TIMEOUT_SECS: u64 = 600;

/// Checks if a URL is accessible by sending a HEAD request
async fn is_url_accessible(url: &Url, client: &Client, cookie_input: Option<&str>) -> Result<()> {
    let mut request = client.head(url.clone());
    if let Some(cookie_header) = cookie_header_value(cookie_input, url)? {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, cookie_header);
        request = request.headers(headers);
    }

    let response = request
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetscapeCookie {
    domain: String,
    include_subdomains: bool,
    path: String,
    secure: bool,
    expires: Option<u64>,
    name: String,
    value: String,
}

/// Builds an HTTP Cookie header value from a raw cookie string or cookies.txt path.
fn cookie_header_from_input(input: &str, url: &Url) -> Result<String> {
    if Path::new(input).is_file() {
        let cookie_file = fs::read_to_string(input)?;
        cookie_header_from_netscape_file(&cookie_file, url)
    } else {
        Ok(input.trim().to_string())
    }
}

fn parse_netscape_cookie(line: &str) -> Option<NetscapeCookie> {
    let line = line.trim();
    let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);

    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() < 7 {
        return None;
    }

    let expires = match fields[4].parse::<u64>().unwrap_or(0) {
        0 => None,
        value => Some(value),
    };

    Some(NetscapeCookie {
        domain: fields[0].trim_start_matches('.').to_ascii_lowercase(),
        include_subdomains: fields[1].eq_ignore_ascii_case("TRUE"),
        path: fields[2].to_string(),
        secure: fields[3].eq_ignore_ascii_case("TRUE"),
        expires,
        name: fields[5].to_string(),
        value: fields[6].to_string(),
    })
}

fn cookie_domain_matches(cookie: &NetscapeCookie, url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let host = host.to_ascii_lowercase();
    host == cookie.domain
        || (cookie.include_subdomains && host.ends_with(&format!(".{}", cookie.domain)))
}

fn cookie_path_matches(cookie: &NetscapeCookie, url: &Url) -> bool {
    let cookie_path = if cookie.path.is_empty() {
        "/"
    } else {
        &cookie.path
    };
    let request_path = url.path();

    request_path == cookie_path
        || request_path
            .strip_prefix(cookie_path)
            .is_some_and(|remainder| cookie_path.ends_with('/') || remainder.starts_with('/'))
}

fn cookie_applies_to_url(cookie: &NetscapeCookie, url: &Url, now: u64) -> bool {
    if let Some(expires) = cookie.expires {
        if expires <= now {
            return false;
        }
    }

    if cookie.secure && url.scheme() != "https" {
        return false;
    }

    cookie_domain_matches(cookie, url) && cookie_path_matches(cookie, url)
}

/// Parses Netscape cookies.txt content into an HTTP Cookie header value.
fn cookie_header_from_netscape_file(content: &str, url: &Url) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| IaGetError::FileSystem(e.to_string()))?
        .as_secs();

    let cookies = content
        .lines()
        .filter_map(parse_netscape_cookie)
        .filter(|cookie| cookie_applies_to_url(cookie, url, now))
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>();

    Ok(cookies.join("; "))
}

fn cookie_header_value(cookie_input: Option<&str>, url: &Url) -> Result<Option<HeaderValue>> {
    let Some(cookie_input) = cookie_input else {
        return Ok(None);
    };

    let cookie_header = cookie_header_from_input(cookie_input, url)?;
    if cookie_header.is_empty() {
        return Ok(None);
    }

    let value = HeaderValue::from_str(&cookie_header)
        .map_err(|e| IaGetError::Network(format!("Invalid cookie header: {}", e)))?;
    Ok(Some(value))
}

/// Converts a details URL to the corresponding XML files list URL
///
/// Takes an archive.org details URL and converts it to the XML metadata URL
/// by replacing "details" with "download" and appending "_files.xml"
///
/// # Arguments
/// * `original_url` - The archive.org details URL
///
/// # Returns
/// The corresponding XML files list URL
fn get_xml_url(original_url: &str) -> String {
    // Remove trailing slash if present to get a consistent base for identifier extraction
    let trimmed_url = original_url.trim_end_matches('/');

    // The identifier is the last segment of the trimmed URL
    // This expect is considered safe because get_xml_url is only called after
    // validate_archive_url has confirmed the URL structure.
    let identifier = trimmed_url
        .rsplit('/')
        .next() // Changed from split().last() to address clippy warning
        .expect("Validated URL should have a valid identifier segment after validation");

    // The base URL for download is "https://archive.org/download/{identifier}"
    let download_url_base = format!("https://archive.org/download/{}", identifier);

    // The XML URL is "{download_url_base}/{identifier}_files.xml"
    format!("{}/{}_files.xml", download_url_base, identifier)
}

/// Fetches and parses XML metadata from archive.org
///
/// Combines XML URL generation, accessibility check, download, and parsing
/// into a single operation with integrated error handling.
///
/// # Arguments
/// * `details_url` - The original archive.org details URL
/// * `client` - HTTP client for requests
/// * `spinner` - Progress spinner to update during processing
///
/// # Returns
/// Tuple of (XmlFiles, base_url) for download processing
async fn fetch_xml_metadata(
    details_url: &str,
    client: &Client,
    spinner: &indicatif::ProgressBar,
    cookie_input: Option<&str>,
) -> Result<(XmlFiles, reqwest::Url, Option<String>)> {
    // Generate XML URL
    let xml_url = get_xml_url(details_url);
    spinner.set_message(format!(
        "{} Accessing XML metadata: {}",
        "⚙".blue(),
        xml_url.bold()
    ));

    // Parse base URL and fetch XML content
    let base_url = reqwest::Url::parse(&xml_url)?;
    let download_cookie_header = cookie_input
        .map(|input| cookie_header_from_input(input, &base_url))
        .transpose()?
        .filter(|header| !header.is_empty());

    // Check XML URL accessibility
    if let Err(e) = is_url_accessible(&base_url, client, cookie_input).await {
        spinner.finish_with_message(format!(
            "{} XML metadata not accessible: {}",
            "✘".red().bold(),
            xml_url.bold()
        ));
        return Err(e); // Propagate the error
    }

    spinner.set_message(format!(
        "{} {}",
        "⚙".blue(),
        "Parsing archive metadata...".bold()
    ));

    let mut request = client.get(base_url.clone());
    if let Some(cookie_header) = download_cookie_header.as_deref() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(cookie_header).map_err(|e| {
                ia_get::IaGetError::Network(format!("Invalid cookie header: {}", e))
            })?,
        );
        request = request.headers(headers);
    }

    let response = request.send().await?;
    let xml_content = response.text().await?;

    // Parse XML content with improved error handling
    let files = parse_xml_files(&xml_content)?;

    Ok((files, base_url, download_cookie_header))
}

/// Return formatted file rows for `--list` output.
fn list_file_rows(files: &XmlFiles) -> Vec<String> {
    files
        .files
        .iter()
        .map(|file| {
            let size = file
                .size
                .map(format_size)
                .unwrap_or_else(|| "unknown".to_string());
            format!("{size:>9} {}", file.name)
        })
        .collect()
}

/// Return a summary for `--list` output.
fn list_summary(files: &XmlFiles) -> String {
    let total_known_size: u64 = files.files.iter().filter_map(|file| file.size).sum();
    let unknown_size_count = files
        .files
        .iter()
        .filter(|file| file.size.is_none())
        .count();
    let file_label = if files.files.len() == 1 {
        "file"
    } else {
        "files"
    };

    if unknown_size_count == 0 {
        format!(
            "{} {file_label}, {} total",
            files.files.len(),
            format_size(total_known_size)
        )
    } else {
        let unknown_label = if unknown_size_count == 1 {
            "unknown size"
        } else {
            "unknown sizes"
        };
        format!(
            "{} {file_label}, {} total known size, {} {unknown_label}",
            files.files.len(),
            format_size(total_known_size),
            unknown_size_count
        )
    }
}

/// Lists parsed filenames from XML metadata when --list/-l is used
fn list_files(files: &XmlFiles, spinner: &indicatif::ProgressBar) {
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template(&format!(
                "{} Archive has {}",
                "✔".green().bold(),
                list_summary(files).bold()
            ))
            .expect("Failed to set completion style"),
    );
    spinner.finish();
    for row in list_file_rows(files) {
        println!("{row}");
    }
}

/// Command-line interface for ia-get
#[derive(Parser)]
#[command(name = "ia-get")]
#[command(about = "A command-line tool for downloading files from the Internet Archive")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(author = env!("CARGO_PKG_AUTHORS"))]
struct Cli {
    /// URL to an archive.org details page
    url: Option<String>,

    /// Include (*apk) or exclude (#jpg) filters by file extension
    #[arg(value_name = "FILTER", num_args = 0..)]
    filters: Vec<String>,

    /// List files parsed from archive metadata XML and exit
    #[arg(short = 'l', long = "list")]
    list: bool,

    /// Cookie header or Netscape cookies.txt file for authenticated downloads
    #[arg(short = 'b', long = "cookies", value_name = "COOKIES")]
    cookies: Option<String>,

    /// Requires include (*ext) or exclude (#ext) filters; ignored without them
    #[arg(
        short = 'k',
        long = "keep-folder-structure",
        visible_aliases = ["folder"],
        conflicts_with = "partial_folder_structure"
    )]
    keep_folder_structure: bool,

    /// Requires include (*ext) or exclude (#ext) filters; keeps paths but only creates folders for downloaded files
    #[arg(
        long = "partial-folder-structure",
        visible_aliases = ["partial-folder", "pk"],
        conflicts_with = "keep_folder_structure"
    )]
    partial_folder_structure: bool,
}

fn normalize_cli_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .map(|arg| {
            let arg = arg.as_ref();
            if arg == "-pk" {
                "--partial-folder-structure".to_string()
            } else {
                arg.to_string()
            }
        })
        .collect()
}

fn parse_cli<I, S>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let normalized = normalize_cli_args(args);
    Cli::try_parse_from(normalized)
}

struct RunRequest {
    url: String,
    filters: Vec<String>,
    list: bool,
    keep_folder_structure: bool,
    partial_folder_structure: bool,
    cookies: Option<String>,
}

fn folder_structure_requires_filters_message(flag: &str) -> String {
    format!("{flag} requires include (*ext) or exclude (#ext) filters")
}

fn validate_folder_structure_flags(
    keep_folder_structure: bool,
    partial_folder_structure: bool,
    has_filters: bool,
) -> Result<()> {
    if (keep_folder_structure || partial_folder_structure) && !has_filters {
        let flag = if keep_folder_structure {
            "keep-folder-structure (-k)"
        } else {
            "partial-folder-structure (-pk)"
        };
        return Err(IaGetError::UrlFormat(folder_structure_requires_filters_message(
            flag,
        )));
    }
    Ok(())
}

fn build_run_request(cli: Cli) -> Result<RunRequest> {
    let url = cli.url.ok_or_else(|| {
        IaGetError::UrlFormat(
            "Missing archive.org URL. Example: ia-get https://archive.org/details/my-item"
                .to_string(),
        )
    })?;

    Ok(RunRequest {
        url,
        filters: cli.filters,
        list: cli.list,
        keep_folder_structure: cli.keep_folder_structure,
        partial_folder_structure: cli.partial_folder_structure,
        cookies: cli.cookies,
    })
}

fn format_filter_summary(filters: &[String]) -> String {
    filters.join(", ")
}

/// Main application entry point
///
/// Parses command line arguments, validates the archive.org URL, checks URL accessibility,
/// downloads XML metadata, and initiates file downloads with built-in signal handling.
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let cli = parse_cli(std::env::args())?;
    let request = build_run_request(cli)?;
    let app_config = Config::load();
    let extension_filters = parse_extension_filters(&request.filters)?;

    let pool_size = if app_config.multithreading {
        app_config.thread_count.max(1)
    } else {
        1
    };

    let client = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(CONNECTION_TIMEOUT_SECS))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(pool_size as usize)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()?;

    let spinner = create_spinner(&format!(
        "Processing archive.org URL: {}",
        request.url.bold()
    ));

    if let Err(e) = validate_archive_url(&request.url) {
        spinner.finish_with_message(format!("{} {}", "✘".red().bold(), e));
        return Err(e.into());
    }

    let details_url = Url::parse(&request.url)?;

    if let Err(e) = is_url_accessible(&details_url, &client, request.cookies.as_deref()).await {
        spinner.finish_with_message(format!(
            "{} Archive.org URL not accessible: {}",
            "✘".red().bold(),
            request.url.bold()
        ));
        return Err(e.into());
    }

    let (mut files, base_url, download_cookie_header) = fetch_xml_metadata(
        &request.url,
        &client,
        &spinner,
        request.cookies.as_deref(),
    )
    .await?;

    let all_archive_paths: Vec<String> = files.files.iter().map(|file| file.name.clone()).collect();
    let has_filters =
        !extension_filters.includes.is_empty() || !extension_filters.excludes.is_empty();

    if let Err(e) = validate_folder_structure_flags(
        request.keep_folder_structure,
        request.partial_folder_structure,
        has_filters,
    ) {
        let flag = if request.keep_folder_structure {
            "keep-folder-structure (-k)"
        } else {
            "partial-folder-structure (-pk)"
        };
        spinner.finish_with_message(format!(
            "{} {}",
            "✘".red().bold(),
            folder_structure_requires_filters_message(flag).bold()
        ));
        return Err(e.into());
    }

    if !extension_filters.includes.is_empty() || !extension_filters.excludes.is_empty() {
        let before = files.files.len();
        files.files.retain(|file| {
            file_passes_extension_filters(
                &file.name,
                &extension_filters.includes,
                &extension_filters.excludes,
            )
        });
        let kept = files.files.len();
        let filter_desc = format_filter_summary(&request.filters);
        spinner.set_message(format!(
            "{} Filtered to {} of {} files matching {}",
            "⚙".blue(),
            kept.to_string().bold(),
            before.to_string().bold(),
            filter_desc.bold()
        ));
    }

    if request.list {
        list_files(&files, &spinner);
        return Ok(());
    }

    if files.files.is_empty() {
        spinner.finish_with_message(format!(
            "{} No files matched the requested filters",
            "✘".red().bold()
        ));
        return Ok(());
    }

    spinner.set_style(
        ProgressStyle::default_spinner()
            .template(&format!(
                "{} {} to download {} files from archive.org {}",
                "✔".green().bold(),
                "Ready".bold(),
                files.files.len().to_string().bold(),
                "★".yellow()
            ))
            .expect("Failed to set completion style"),
    );
    spinner.finish();

    if !has_filters || request.keep_folder_structure {
        let archive_dirs = collect_archive_directories(&all_archive_paths);
        if !archive_dirs.is_empty() {
            create_archive_directories(&archive_dirs)?;
        }
    } else if request.partial_folder_structure {
        let filtered_paths: Vec<String> = files.files.iter().map(|file| file.name.clone()).collect();
        let archive_dirs = collect_archive_directories(&filtered_paths);
        if !archive_dirs.is_empty() {
            create_archive_directories(&archive_dirs)?;
        }
    }

    let mut sanitized_count = 0;
    let download_data = files
        .files
        .into_iter()
        .map(|file| {
            let mut absolute_url = base_url.clone();
            if let Ok(joined_url) = absolute_url.join(&file.name) {
                absolute_url = joined_url;
            }

            let (local_path, was_modified) = resolve_local_download_path(
                &file.name,
                request.keep_folder_structure,
                request.partial_folder_structure,
                has_filters,
            );

            if was_modified {
                println!(
                    "{} {} {} → {}",
                    "⚠".yellow().bold(),
                    "Sanitized:".yellow(),
                    file.name.dimmed(),
                    local_path.bold()
                );
                sanitized_count += 1;
            }

            (absolute_url.to_string(), local_path, file.md5)
        })
        .collect::<Vec<_>>();

    if sanitized_count > 0 {
        println!(
            "\n{} {} {} file{} for filesystem compatibility",
            "✓".green().bold(),
            "Sanitized".bold(),
            sanitized_count.to_string().bold(),
            if sanitized_count == 1 { "" } else { "s" }
        );
    }

    let download_options = DownloadOptions {
        max_bytes_per_sec: config::max_bytes_per_second(app_config.max_bandwidth_kbps),
        multithreading: app_config.multithreading,
        thread_count: app_config.thread_count,
    };

    downloader::download_files(
        &client,
        download_data.clone(),
        download_data.len(),
        download_cookie_header.as_deref(),
        download_options,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ia_get::utils::validate_archive_url;

    #[test]
    fn cookie_header_accepts_raw_cookie_string() {
        assert_eq!(
            cookie_header_from_input(
                "logged-in-user=yes; logged-in-sig=abc123",
                &cookie_test_url("/download/item/item_files.xml"),
            )
            .unwrap(),
            "logged-in-user=yes; logged-in-sig=abc123"
        );
    }

    #[test]
    fn cookie_header_parses_netscape_cookie_file_content() {
        let cookies = "# Netscape HTTP Cookie File\n\
.archive.org\tTRUE\t/\tFALSE\t2145916800\tlogged-in-user\tyes\n\
archive.org\tFALSE\t/\tTRUE\t2145916800\tlogged-in-sig\tabc123\n";

        assert_eq!(
            cookie_header_from_netscape_file(
                cookies,
                &cookie_test_url("/download/item/item_files.xml")
            )
            .unwrap(),
            "logged-in-user=yes; logged-in-sig=abc123"
        );
    }

    #[test]
    fn cookie_header_respects_domain_and_path_scoping() {
        let cookies = "# Netscape HTTP Cookie File\n\
.archive.org\tTRUE\t/download\tFALSE\t2145916800\tdownload-root\tyes\n\
archive.org\tFALSE\t/account\tFALSE\t2145916800\taccount-only\tnope\n\
example.com\tFALSE\t/download\tFALSE\t2145916800\twrong-domain\tnope\n\
archive.org\tFALSE\t/download/private\tFALSE\t2145916800\tprivate-only\tsecret\n";

        assert_eq!(
            cookie_header_from_netscape_file(cookies, &cookie_test_url("/download/item/file.zip"))
                .unwrap(),
            "download-root=yes"
        );

        assert_eq!(
            cookie_header_from_netscape_file(
                cookies,
                &cookie_test_url("/download/private/file.zip")
            )
            .unwrap(),
            "download-root=yes; private-only=secret"
        );
    }

    #[test]
    fn cookie_header_ignores_expired_netscape_cookies() {
        let cookies = "archive.org\tFALSE\t/\tFALSE\t1\told\tvalue\n\
archive.org\tFALSE\t/\tFALSE\t2145916800\tcurrent\tvalue\n";

        assert_eq!(
            cookie_header_from_netscape_file(
                cookies,
                &cookie_test_url("/download/item/item_files.xml")
            )
            .unwrap(),
            "current=value"
        );
    }

    fn cookie_test_url(path: &str) -> Url {
        Url::parse(&format!("https://archive.org{path}")).unwrap()
    }

    #[test]
    fn cli_parses_partial_folder_flag_after_filters() {
        let cli = parse_cli([
            "ia-get",
            "https://archive.org/details/yw1_rom.app/",
            "*ipa",
            "-pk",
        ])
        .expect("CLI should parse -pk as partial-folder-structure, not as a filter");

        assert!(cli.partial_folder_structure);
        assert!(!cli.keep_folder_structure);
        assert_eq!(cli.filters, vec!["*ipa".to_string()]);
    }

    #[test]
    fn cli_rejects_keep_and_partial_folder_flags_together() {
        let result = parse_cli([
            "ia-get",
            "https://archive.org/details/yw1_rom.app/",
            "*ipa",
            "-k",
            "-pk",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_parses_keep_folder_flag_after_filters() {
        let cli = Cli::try_parse_from([
            "ia-get",
            "https://archive.org/details/yw1_rom.app/",
            "*ipa",
            "-k",
        ])
        .expect("CLI should parse -k as keep-folder-structure, not as a filter");

        assert!(cli.keep_folder_structure);
        assert_eq!(cli.filters, vec!["*ipa".to_string()]);
        assert_eq!(
            cli.url.as_deref(),
            Some("https://archive.org/details/yw1_rom.app/")
        );
    }

    #[test]
    fn cli_parses_keep_folder_long_flag_after_filters() {
        let cli = Cli::try_parse_from([
            "ia-get",
            "https://archive.org/details/item",
            "*apk",
            "--keep-folder-structure",
        ])
        .expect("CLI should parse long keep-folder flag after filters");

        assert!(cli.keep_folder_structure);
        assert_eq!(cli.filters, vec!["*apk".to_string()]);
    }

    #[test]
    fn check_valid_pattern() {
        assert!(validate_archive_url("https://archive.org/details/Valid-Pattern").is_ok());
        assert!(validate_archive_url("https://archive.org/details/Valid-Pattern/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test123").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test123/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test_file-name.data").is_ok());
        assert!(validate_archive_url("https://archive.org/details/test_file-name.data/").is_ok());
        assert!(validate_archive_url("https://archive.org/details/user@domain").is_ok());
        assert!(validate_archive_url("https://archive.org/details/user@domain/").is_ok());
    }

    #[test]
    fn check_invalid_pattern() {
        assert!(validate_archive_url("https://archive.org/details/Invalid-Pattern-*").is_err());
        assert!(validate_archive_url("https://archive.org/details/").is_err()); // This should still be an error (empty identifier)
        assert!(validate_archive_url("https://example.com/details/test").is_err());
        assert!(validate_archive_url("http://archive.org/details/test").is_err());
        assert!(validate_archive_url("https://archive.org/details/test/extra").is_err());
        assert!(validate_archive_url("https://archive.org/details/test//").is_err());
        // Multiple trailing slashes
    }

    #[test]
    fn check_get_xml_url() {
        assert_eq!(
            get_xml_url("https://archive.org/details/item1"),
            "https://archive.org/download/item1/item1_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/item1/"), // With trailing slash
            "https://archive.org/download/item1/item1_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/another-item_v2.0"),
            "https://archive.org/download/another-item_v2.0/another-item_v2.0_files.xml"
        );
        assert_eq!(
            get_xml_url("https://archive.org/details/another-item_v2.0/"), // With trailing slash
            "https://archive.org/download/another-item_v2.0/another-item_v2.0_files.xml"
        );
    }

    fn xml_file(name: &str, size: Option<u64>) -> ia_get::archive_metadata::XmlFile {
        ia_get::archive_metadata::XmlFile {
            name: name.to_string(),
            source: "original".to_string(),
            mtime: None,
            size,
            format: None,
            rotation: None,
            md5: None,
            crc32: None,
            sha1: None,
            btih: None,
            summation: None,
            original: None,
        }
    }

    #[test]
    fn list_file_rows_format_sizes_and_unknown_entries() {
        let files = XmlFiles {
            files: vec![
                xml_file("cover.jpg", Some(12_345)),
                xml_file("metadata.xml", None),
            ],
        };

        assert_eq!(
            list_file_rows(&files),
            vec![
                "  12.06KB cover.jpg".to_string(),
                "  unknown metadata.xml".to_string(),
            ]
        );
    }

    #[test]
    fn list_summary_reports_total_known_size_and_unknown_count() {
        let files = XmlFiles {
            files: vec![
                xml_file("disk1.zip", Some(1_048_576)),
                xml_file("disk2.zip", Some(2_097_152)),
                xml_file("notes.txt", None),
            ],
        };

        assert_eq!(
            list_summary(&files),
            "3 files, 3.00MB total known size, 1 unknown size"
        );
    }
}
