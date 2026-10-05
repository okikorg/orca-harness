//! How a user message with images is written to a session line and read
//! back.
//!
//! Core never serializes `Message::User.images`, so the line is written
//! here in `Message`'s own shape. An image the provider fetches by URL is
//! not kept in `images` but in a separate `image_urls` list with its
//! position: a build from before URL images reads every `images` entry as
//! base64, and would send an empty `data:` URL that providers reject. It
//! ignores the unknown `image_urls` field instead, so it loses only those
//! images. This build puts each one back where it was.

use serde::{Deserialize, Serialize};

use orca_harness_core::{Image, Message};

#[derive(Serialize)]
enum Record<'a> {
    User {
        content: &'a str,
        images: Vec<&'a Image>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        image_urls: Vec<UrlImage>,
    },
}

#[derive(Serialize, Deserialize)]
struct UrlImage {
    /// Index in the message's full image list.
    at: usize,
    url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    media_type: String,
}

#[derive(Deserialize)]
enum Urls {
    User {
        #[serde(default)]
        image_urls: Vec<UrlImage>,
    },
}

/// One session line for `message`.
pub(super) fn line(message: &Message) -> serde_json::Result<String> {
    let Message::User { content, images } = message else {
        return serde_json::to_string(message);
    };
    if images.is_empty() {
        return serde_json::to_string(message);
    }
    let mut inline = Vec::new();
    let mut image_urls = Vec::new();
    for (at, image) in images.iter().enumerate() {
        // An image with bytes of its own is sent inline, so it is stored
        // inline too, where any build can read it.
        match image.source_url.as_ref().filter(|_| image.data.is_empty()) {
            Some(url) => image_urls.push(UrlImage {
                at,
                url: url.clone(),
                media_type: image.media_type.clone(),
            }),
            None => inline.push(image),
        }
    }
    serde_json::to_string(&Record::User {
        content,
        images: inline,
        image_urls,
    })
}

/// Put the URL images `line` recorded back among `message`'s inline ones.
pub(super) fn restore(line: &str, message: &mut Message) -> serde_json::Result<()> {
    let Message::User { images, .. } = message else {
        return Ok(());
    };
    if !line.contains("\"image_urls\"") {
        return Ok(());
    }
    let Urls::User { image_urls } = serde_json::from_str(line)?;
    // Written in ascending order, so each lands at its original index.
    for UrlImage {
        at,
        url,
        media_type,
    } in image_urls
    {
        let image = Image {
            media_type,
            data: String::new(),
            source_url: Some(url),
        };
        images.insert(at.min(images.len()), image);
    }
    Ok(())
}
