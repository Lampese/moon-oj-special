// moon: The build system and package manager for MoonBit.
// Copyright (C) 2024 International Digital Economy Academy
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// For inquiries, you can contact us via e-mail at jichuruanjian@idea.edu.cn.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use console::Term;
use dialoguer::Confirm;
use futures::stream::{self, StreamExt, TryStreamExt};
use moonutil::common::{
    get_moon_version, get_moonc_version, get_moonrun_version, CargoPathExt, VersionItems,
    MOONBITLANG_CORE,
};
use moonutil::moon_dir::{self, moon_tmp_dir};
use reqwest;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio;
use tokio::io::AsyncWriteExt;
use tokio::signal;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
use tokio::fs::set_permissions;

#[derive(Debug, clap::Parser, Clone)]
pub struct UpgradeSubcommand {
    /// Force upgrade
    #[clap(long, short)]
    pub force: bool,
}

#[derive(Default)]
struct DownloadProgress {
    total_size: u64,
    downloaded: u64,
}

/// Copy from: https://github.com/rust-lang/cargo/blob/c21dd51/crates/cargo-util/src/paths.rs#L84
///
/// Normalize a path, removing things like `.` and `..`.
///
/// CAUTION: This does not resolve symlinks (unlike
/// [`std::fs::canonicalize`]). This may cause incorrect or surprising
/// behavior at times. This should be used carefully. Unfortunately,
/// [`std::fs::canonicalize`] can be hard to use correctly, since it can often
/// fail, or on Windows returns annoying device paths. This is a problem Cargo
/// needs to improve on.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

fn check_connectivity() -> anyhow::Result<&'static str> {
    let url = "https://www.google.com";

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("Failed to create HTTP client")?;

    let resp = client.get(url).send();
    if resp.is_ok() && resp.unwrap().status().is_success() {
        Ok("https://cli.moonbitlang.com")
    } else {
        Ok("https://cli.moonbitlang.cn")
    }
}

fn os_arch() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "macos") => "macos_intel",
        ("aarch64", "macos") => "macos_m1",
        ("x86_64", "linux") => "ubuntu_x86",
        ("x86_64", "windows") => "windows",
        _ => panic!("unsupported platform"),
    }
}

fn extract_date(input: &str) -> Option<String> {
    // from the second dot
    input.split('.').nth(2).and_then(|s| {
        // find the first digit
        let start = s.find(|c: char| c.is_ascii_digit())?;
        // take 8 chars from the start
        let date = s[start..].chars().take(8).collect::<String>();
        // ensure the extracted string is 8 chars and all digits
        if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) {
            Some(date)
        } else {
            None
        }
    })
}

#[test]
fn test_extract_date() {
    let date1 = extract_date("0.1.20240828 (901ac075 2024-08-28)").unwrap();
    assert_eq!("20240828", date1);
    let date2 = extract_date("v0.1.20240827+848d2bb76").unwrap();
    assert_eq!("20240827", date2);
    assert!(date1 > date2);
}

fn should_upgrade(latest_version_info: &VersionItems) -> Option<bool> {
    let moon_version = get_moon_version();
    let moonrun_version = get_moonrun_version().ok()?;
    let moonc_version = get_moonc_version().ok()?;

    // extract date from moon_version and moonc_version, compare with latest
    let moon_date = extract_date(&moon_version)?;
    let moonrun_date = extract_date(&moonrun_version)?;
    let moonc_date = extract_date(&moonc_version)?;
    let mut should_upgrade = false;
    for item in &latest_version_info.items {
        let latest_date = extract_date(&item.version)?;

        if ((item.name == "moon") && latest_date > moon_date)
            || (item.name == "moonrun" && latest_date > moonrun_date)
            || (item.name == "moonc" && latest_date > moonc_date)
        {
            should_upgrade = true;
        }
    }

    Some(should_upgrade)
}

pub fn upgrade(cmd: UpgradeSubcommand) -> Result<i32> {
    ctrlc::set_handler(moonutil::common::dialoguer_ctrlc_handler)?;

    let h = moon_dir::home();

    println!("Checking network ...");
    let root = check_connectivity()?;
    println!("  Use {}", root);

    let download_page = if root.contains("moonbitlang.cn") {
        "https://www.moonbitlang.cn/download"
    } else {
        "https://www.moonbitlang.com/download"
    };

    println!("Checking latest toolchain version ...");
    let version_url = format!("{}/version.json", root);
    if !cmd.force {
        // if any step(network request, serde json...) fail, just do upgrade
        if let Ok(data) = reqwest::blocking::get(version_url) {
            if let Ok(latest_version_info) = data.json::<VersionItems>() {
                if let Some(false) = should_upgrade(&latest_version_info) {
                    println!("Your toolchain is up to date.");
                    return Ok(0);
                }
            }
        }
    }

    println!("{}", "Warning: moon upgrade is highly experimental.".bold());
    let msg = format!(
        "If you encounter any problems, please reinstall by visit {}",
        download_page
    );
    println!("{}", msg.bold());
    let confirm = Confirm::new()
        .with_prompt(format!(
            "Will install to {}. Continue?",
            h.display().to_string().bold()
        ))
        .default(true)
        .interact()?;
    if confirm {
        do_upgrade(root)?;
    }
    println!("{}", "Done".green().bold());
    Ok(0)
}

pub fn do_upgrade(root: &'static str) -> Result<i32> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let items = [
            "include/moonbit.h",
            "include/moonbit-fundamental.h",
            "lib/libmoonbitrun.o",
            "lib/libtcc1.a",
            "bin/moon",
            "bin/moonc",
            "bin/moonfmt",
            "bin/moonrun",
            "bin/mooninfo",
            "bin/moondoc",
            "bin/moon_cove_report",
            "bin/mooncake",
            "bin/internal/tcc",
            "core.zip",
        ];
        let download_items_and_urls = items
            .iter()
            .map(|item| {
                if *item != "core.zip" {
                    (item.to_string(), format!("{}/{}/{}{}", root, os_arch(), item, if os_arch() == "windows" && !item.contains(".") { ".exe" } else { "" }))
                } else {
                    (item.to_string(), format!("{}/{}", root, item))
                }
            })
            .collect::<Vec<(String,String)>>();

        let temp_dir = tempfile::tempdir_in(moon_tmp_dir()?)?;
        let temp_dir_path = temp_dir.path();

        let progress_map = Arc::new(Mutex::new(indexmap::map::IndexMap::new()));

        let term = Arc::new(Mutex::new(Term::stdout()));

        for (download_item, _) in download_items_and_urls.iter() {
            let mut map = progress_map.lock().unwrap();
            map.insert(
                download_item,
                DownloadProgress {
                    total_size: 0,
                    downloaded: 0,
                },
            );
        }

        let download_futures = download_items_and_urls.iter().map(|(download_item, url)| {
            let progress_map = Arc::clone(&progress_map);
            let term = Arc::clone(&term);
            async move {
                let filepath = temp_dir_path.join(download_item);
                if let Some(parent) = filepath.parent() {
                    if !parent.exists() {
                        tokio::fs::create_dir_all(parent).await.context(format!("failed to create directory {}", parent.display()))?;
                    }
                }
                let response = reqwest::get(url).await.context(format!("failed to download {}", download_item))?;
                let total_size = response.content_length().context(format!("failed to download {}: No content length", download_item))?;
                let mut file = tokio::fs::File::create(&filepath)
                    .await
                    .context(format!("failed to create file {}", filepath.display()))?;

                {
                    let mut map = progress_map.lock().unwrap();
                    map.insert(
                        download_item,
                        DownloadProgress {
                            total_size,
                            downloaded: 0,
                        },
                    );
                }

                let mut stream = response.bytes_stream();
                while let Some(item) = stream.next().await {
                    let chunk = item.context(format!("error while downloading {}", download_item))?;
                    file.write_all(&chunk)
                        .await
                        .context(format!("error while writing to file {}", filepath.display()))?;

                    {
                        let mut map = progress_map.lock().unwrap();
                        if let Some(progress) = map.get_mut(download_item) {
                            progress.downloaded += chunk.len() as u64;
                        }
                    }
                    display_progress(&term, &progress_map);
                }

                file.flush().await.context(format!("failed to flush file {}", filepath.display()))?;
                Ok::<(), anyhow::Error>(())
            }
        });

        let downloads = stream::iter(download_futures)
            .map(Ok)
            .try_for_each_concurrent(None, |f| f);

        // Listen for Ctrl+C
        let ctrl_c_handling = signal::ctrl_c();

        // Use tokio::select! to wait for either downloads completion or Ctrl+C signal
        tokio::select! {
            _ = ctrl_c_handling => {
                bail!("upgrade interrupted by Ctrl+C");
            },
            result = downloads => {
                result?;

                println!();

                // post handling
                for (download_item, _) in download_items_and_urls {
                    let filepath = temp_dir_path.join(&download_item);
                    match filepath.extension().and_then(std::ffi::OsStr::to_str) {
                        Some("zip") => {
                            // delete old
                            let lib_dir = moon_dir::home().join("lib");
                            let core_dir = lib_dir.join("core");
                            core_dir.rm_rf();

                            // unzip
                            let data = tokio::fs::read(&filepath).await.context(format!("failed to read {}", filepath.display()))?;
                            let cursor = std::io::Cursor::new(data);
                            let mut zip = zip::ZipArchive::new(cursor)?;
                            for i in 0..zip.len() {
                                let mut file = zip.by_index(i)?;
                                let outpath = lib_dir.join(file.mangled_name());

                                if file.is_dir() {
                                    std::fs::create_dir_all(&outpath)?;
                                } else {
                                    if let Some(parent) = outpath.parent() {
                                        std::fs::create_dir_all(parent)?;
                                    }
                                    let mut outfile = std::fs::File::create(&outpath)?;
                                    std::io::copy(&mut file, &mut outfile)?;
                                }
                            }

                            // use new moon to bundle
                            let moon = moon_dir::home().join("bin").join("moon");
                            println!("Compiling {} ...", MOONBITLANG_CORE);
                            let out = std::process::Command::new(&moon).args(["version"]).output()?;
                            println!("moon version: {}", String::from_utf8_lossy(&out.stdout));

                            let out = std::process::Command::new(moon).args(["bundle", "--all", "--source-dir", &core_dir.display().to_string()]).output()?;
                            println!("{}", String::from_utf8_lossy(&out.stdout));
                            match out.status.code() {
                                Some(0) => {},
                                Some(code) => bail!("failed to compile core, exit code {}", code),
                                None => bail!("failed to bundle {}", MOONBITLANG_CORE),

                            }
                        }
                        _ => {
                            let dst = moon_dir::home().join(download_item);
                            if let Some(parent) = dst.parent() {
                                if !parent.exists() {
                                    tokio::fs::create_dir_all(parent).await.context(format!("failed to create directory {}", parent.display()))?;
                                }
                            }
                            let msg = format!("failed to copy {}", dst.display());
                            let cur_bin = std::env::current_exe().context("failed to get current executable")?;
                            let cur_bin_norm = normalize_path(&cur_bin);
                            let dst_norm = normalize_path(&dst);
                            let replace_self = dst_norm == cur_bin_norm;
                            if replace_self {
                                self_replace::self_replace(&filepath).context(format!("failed to replace {}", cur_bin.display()))?;
                                tokio::fs::remove_file(&filepath).await.context(format!("failed to remove {}", filepath.display()))?;
                            } else {
                                if dst.exists() {
                                    tokio::fs::remove_file(&dst).await.context(format!("failed to remove {}", dst.display()))?;
                                }
                                tokio::fs::copy(&filepath, &dst)
                                    .await
                                    .with_context(|| msg)?;
                            }

                            #[cfg(unix)]
                            {
                                let mut perms = tokio::fs::metadata(&dst).await.context(format!("failed to get metadata of {}", dst.display()))?.permissions();
                                perms.set_mode(0o744);
                                set_permissions(&dst, perms)
                                    .await
                                    .context(format!("failed to set execute permissions for {}", filepath.display()))?;
                            }
                        }
                    }
                }

                let _ = term.lock().unwrap().write_line("");
                Ok(0)
            },
        }
    })
}

fn display_progress(
    term: &Arc<Mutex<Term>>,
    progress_map: &Arc<Mutex<indexmap::map::IndexMap<&String, DownloadProgress>>>,
) {
    let map = progress_map.lock().unwrap();

    let mut cur = 0.0;
    let mut total = 0.0;
    map.iter().for_each(|(_url, progress)| {
        cur += progress.downloaded as f64;
        total += progress.total_size as f64;
    });

    let msg = format!("Downloading {:.1}%", cur / total * 100.0);

    {
        let mut term = term.lock().unwrap();
        let _ = term.clear_line();
        let _ = term.write(msg.as_bytes());
    }
}
