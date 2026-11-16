//! Images in agent messages. Paseo turns the images of tool results into markdown that points at
//! a file on the daemon's machine, `![Image](file:///…/paseo-attachments-…/<sha256>.png)`. Zed's
//! agent view shows images from absolute paths but not `file://` URLs, so the links are
//! rewritten to a path on this machine: the file itself when it is here (a daemon on this
//! machine), else a copy in Pazedo's cache, read through the daemon as the Paseo app does.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use collections::HashSet;
use gpui::{App, AppContext as _, Entity, Global};
use paseo_client::protocol::{AgentStreamEvent, ReadFile, TimelineItem};
use util::ResultExt as _;

use crate::PaseoStore;

/// An image on the daemon's machine and where its copy goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteImage {
    pub remote: PathBuf,
    pub local: PathBuf,
}

/// Rewrites the `file://` image links in `text` to local paths, adding the images that still
/// have to be fetched to `images`. `None` when there is nothing to rewrite.
pub(crate) fn localize_image_links(
    text: &str,
    cache_dir: &Path,
    images: &mut Vec<RemoteImage>,
) -> Option<String> {
    if !text.contains("](file://") {
        return None;
    }
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(start) = rest.find("![") {
        let Some(source_start) = rest[start..].find("](").map(|offset| start + offset + 2) else {
            break;
        };
        let Some(source_length) = link_destination_length(&rest[source_start..]) else {
            break;
        };
        let source_end = source_start + source_length;
        let source = rest[source_start..source_end]
            .replace("\\)", ")")
            .replace("\\\\", "\\");
        let local = source
            .starts_with("file://")
            .then(|| url::Url::parse(&source).ok()?.to_file_path().ok())
            .flatten()
            .map(|remote| {
                if remote.exists() {
                    remote
                } else {
                    let local = cache_path(cache_dir, &remote);
                    if !local.exists() && !images.iter().any(|image| image.local == local) {
                        images.push(RemoteImage {
                            remote,
                            local: local.clone(),
                        });
                    }
                    local
                }
            });
        result.push_str(&rest[..source_start]);
        match local {
            Some(local) => {
                // Angle brackets keep a path with spaces or parentheses one destination.
                result.push('<');
                result.push_str(&local.to_string_lossy());
                result.push('>');
                changed = true;
            }
            None => result.push_str(&rest[source_start..source_end]),
        }
        rest = &rest[source_end..];
    }
    result.push_str(rest);
    changed.then_some(result)
}

/// An assistant message with its image links rewritten (see [`localize_image_links`]).
pub(crate) fn localize_item(
    item: TimelineItem,
    cache_dir: &Path,
    images: &mut Vec<RemoteImage>,
) -> TimelineItem {
    match item {
        TimelineItem::AssistantMessage { text } => TimelineItem::AssistantMessage {
            text: localize_image_links(&text, cache_dir, images).unwrap_or(text),
        },
        item => item,
    }
}

pub(crate) fn localize_event(
    event: AgentStreamEvent,
    cache_dir: &Path,
    images: &mut Vec<RemoteImage>,
) -> AgentStreamEvent {
    match event {
        AgentStreamEvent::Timeline { item, turn_id } => AgentStreamEvent::Timeline {
            item: localize_item(item, cache_dir, images),
            turn_id,
        },
        event => event,
    }
}

/// The length of a link destination up to its closing parenthesis, which a backslash escapes.
fn link_destination_length(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            ')' => return Some(index),
            '\n' => return None,
            _ => {}
        }
    }
    None
}

/// Where the copy of a remote image goes. Paseo names its images after their content hash;
/// the remote path's hash keeps other files with the same name apart.
fn cache_path(cache_dir: &Path, remote: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    remote.hash(&mut hasher);
    let name = remote
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    cache_dir.join(format!("{:016x}-{name}", hasher.finish()))
}

pub(crate) fn cache_dir() -> PathBuf {
    paths::temp_dir().join("paseo-images")
}

/// The images being fetched, so that a message rendered again does not fetch them twice.
#[derive(Default)]
struct FetchingImages(HashSet<PathBuf>);

impl Global for FetchingImages {}

/// Copies the images from the daemon's machine into the cache, then redraws the windows so the
/// messages that show them pick them up.
pub(crate) fn fetch_images(store: &Entity<PaseoStore>, images: Vec<RemoteImage>, cx: &mut App) {
    if images.is_empty() {
        return;
    }
    let Some(client) = store.read(cx).client() else {
        return;
    };
    let fetching = &mut cx.default_global::<FetchingImages>().0;
    let images: Vec<_> = images
        .into_iter()
        .filter(|image| fetching.insert(image.local.clone()))
        .collect();
    for image in images {
        let client = client.clone();
        cx.spawn(async move |cx| {
            let result = fetch_image(&client, &image, cx).await;
            cx.update(|cx| {
                cx.default_global::<FetchingImages>().0.remove(&image.local);
                if result.is_ok() {
                    cx.refresh_windows();
                }
            });
            result
                .with_context(|| format!("failed to fetch the image {}", image.remote.display()))
                .log_err();
        })
        .detach();
    }
}

async fn fetch_image(
    client: &paseo_client::Client,
    image: &RemoteImage,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let (Some(directory), Some(name)) = (image.remote.parent(), image.remote.file_name()) else {
        bail!("not a file path");
    };
    let response = client
        .request(&ReadFile::new(
            directory.to_string_lossy(),
            name.to_string_lossy(),
        ))
        .await?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    let file = response.file.context("the daemon sent no file")?;
    if file.encoding != "base64" {
        bail!(
            "expected a base64 image, got {} ({})",
            file.kind,
            file.encoding
        );
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(file.content.unwrap_or_default())
        .context("invalid base64")?;
    let local = image.local.clone();
    cx.background_spawn(async move {
        let directory = local.parent().context("no cache directory")?;
        std::fs::create_dir_all(directory)?;
        // Written aside first, so a half-written file never shows.
        let partial = local.with_extension("partial");
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, &local)?;
        anyhow::Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_file_links_to_local_paths() {
        let cache = Path::new("/cache");
        let mut images = Vec::new();
        assert_eq!(
            localize_image_links("no images here", cache, &mut images),
            None
        );
        assert_eq!(
            localize_image_links("![web](https://example.com/a.png)", cache, &mut images),
            None
        );

        let remote = "/var/folders/xy/T/paseo-attachments-AbC/0123abcd.png";
        let text = format!("Look:\n\n![Image](file://{remote})\n\nand ![x](a.png).");
        let rewritten = localize_image_links(&text, cache, &mut images).unwrap();
        let [image] = images.as_slice() else {
            panic!("expected one image to fetch, got {images:?}");
        };
        assert_eq!(image.remote, Path::new(remote));
        assert!(image.local.starts_with(cache));
        assert!(image.local.to_string_lossy().ends_with("-0123abcd.png"));
        assert_eq!(
            rewritten,
            format!(
                "Look:\n\n![Image](<{}>)\n\nand ![x](a.png).",
                image.local.display()
            )
        );

        // The same image again is fetched once.
        localize_image_links(&text, cache, &mut images);
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn keeps_local_files_and_decodes_their_urls() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("a b.png");
        std::fs::write(&file, b"png").unwrap();
        let url = url::Url::from_file_path(&file).unwrap();
        let mut images = Vec::new();
        let rewritten = localize_image_links(
            &format!("![Image]({url})"),
            Path::new("/cache"),
            &mut images,
        )
        .unwrap();
        assert!(images.is_empty(), "a file on this machine needs no copy");
        assert_eq!(rewritten, format!("![Image](<{}>)", file.display()));
    }
}
