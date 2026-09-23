//! The kinds of files, in one place (PLAN.md §66): which extensions are
//! pictures, documents, music, videos and archives. The filters (Images,
//! Documents…), the importance of a file, OCR, photo places and tags,
//! properties and the icons' colours all read these lists, so they agree.
//!
//! Pictures are the formats Windows itself decodes (WIC, with the free HEIF,
//! WebP, Raw and JPEG XL extensions that Windows 11 installs): whereti never
//! parses a picture with its own code.

/// What a file is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Picture,
    Document,
    Audio,
    Video,
    Archive,
}

/// Pictures Windows decodes: OCR, preview, the photo model.
pub const PICTURES: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "bmp", "dib", "gif", "tif", "tiff", "webp", "heic", "heif", "hif", "avif",
    "jxl", "jxr", "wdp", "ico",
];

/// Camera raw files (Windows' Raw Image Extension decodes them).
pub const RAW: &[&str] = &[
    // not "cap" (network captures share it)
    "dng", "3fr", "ari", "arw", "bay", "cr2", "cr3", "crw", "dcs", "dcr", "drf", "eip", "erf", "fff", "iiq",
    "k25", "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "ptx", "pxn", "raf", "raw", "rw2", "rwl",
    "sr2", "srf", "srw", "x3f",
];

/// Other image files (drawings, layered and vector files): found by the
/// Images filter, not read.
pub const OTHER_IMAGES: &[&str] = &["svg", "psd", "psb", "ai", "eps", "xcf", "kra", "cdr", "afphoto", "afdesign", "dds", "tga", "exr", "hdr", "cur"];

/// Photos as cameras and phones write them, with EXIF (date, camera, GPS).
pub const PHOTOS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "heic", "heif", "hif", "avif", "jxl", "tif", "tiff", "png", "webp"];

/// Documents for the Documents filter (some are found by name only: their
/// text is read only for the formats `whereti-extract` knows).
pub const DOCUMENTS: &[&str] = &[
    // word processing
    // not "dot" (Graphviz) or "pot" (gettext): their Office meanings are rare
    "pdf", "doc", "docx", "docm", "dotx", "dotm", "odt", "ott", "rtf", "txt", "md", "markdown", "pages", "wpd", "wps", "hwp",
    // spreadsheets
    "xls", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "ods", "ots", "csv", "tsv", "numbers",
    // presentations
    // not "key" (Keynote) or "pub" (Publisher): certificates and SSH keys share them
    "ppt", "pptx", "pptm", "pps", "ppsx", "ppsm", "potx", "potm", "odp", "otp",
    // books and fixed layout
    "epub", "mobi", "azw", "azw3", "fb2", "djvu", "xps", "oxps",
    // mail and notes
    "eml", "msg", "one", "vsd", "vsdx", "tex",
];

pub const AUDIO: &[&str] = &[
    "mp3", "wav", "flac", "aac", "ogg", "oga", "opus", "m4a", "m4b", "wma", "aiff", "aif", "ape", "wv", "mka", "mid",
    "midi", "amr", "ac3", "dts", "mpc", "spx",
];

pub const VIDEO: &[&str] = &[
    // not "ts": TypeScript is far more common on a PC than MPEG transport streams
    "mp4", "m4v", "mov", "mkv", "avi", "wmv", "webm", "mpg", "mpeg", "m2ts", "mts", "3gp", "3g2", "flv", "vob", "ogv", "asf",
];

/// Music whose tags and length whereti reads.
pub const AUDIO_TAGGED: &[&str] = &["mp3", "flac", "m4a", "m4b", "aac", "ogg", "oga", "opus", "wav", "wma", "aiff", "aif", "ape", "wv", "mpc", "spx"];
/// Videos whose length whereti reads (the MP4 family by lofty, the others by
/// their headers).
pub const VIDEO_TAGGED: &[&str] = &["mp4", "m4v", "mov", "3gp", "3g2", "mkv", "webm", "avi", "wmv", "asf"];

pub const ARCHIVES: &[&str] = &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso"];

/// The category of a lower-case extension.
pub fn category(ext: &str) -> Option<Category> {
    let is = |list: &[&str]| list.contains(&ext);
    if is(PICTURES) || is(RAW) || is(OTHER_IMAGES) {
        Some(Category::Picture)
    } else if is(DOCUMENTS) {
        Some(Category::Document)
    } else if is(AUDIO) {
        Some(Category::Audio)
    } else if is(VIDEO) {
        Some(Category::Video)
    } else if is(ARCHIVES) {
        Some(Category::Archive)
    } else {
        None
    }
}

/// `ext:a;b;c` for every extension of a category (the filters).
pub fn ext_clause(category: Category) -> String {
    let lists: &[&[&str]] = match category {
        Category::Picture => &[PICTURES, RAW, OTHER_IMAGES],
        Category::Document => &[DOCUMENTS],
        Category::Audio => &[AUDIO],
        Category::Video => &[VIDEO],
        Category::Archive => &[ARCHIVES],
    };
    format!("ext:{}", lists.iter().flat_map(|l| l.iter()).copied().collect::<Vec<_>>().join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_lower_case_and_do_not_overlap() {
        let all: Vec<&[&str]> = vec![PICTURES, RAW, OTHER_IMAGES, DOCUMENTS, AUDIO, VIDEO, ARCHIVES];
        let mut seen = std::collections::HashSet::new();
        for list in &all {
            for e in *list {
                assert_eq!(*e, e.to_lowercase(), "{e}");
                assert!(seen.insert(*e), "{e} is in two lists");
            }
        }
        assert!(PHOTOS.iter().all(|e| PICTURES.contains(e)));
        assert!(AUDIO_TAGGED.iter().all(|e| AUDIO.contains(e)));
        assert!(VIDEO_TAGGED.iter().all(|e| VIDEO.contains(e)));
        assert_eq!(category("cr3"), Some(Category::Picture));
        assert_eq!(category("epub"), Some(Category::Document));
        assert!(ext_clause(Category::Picture).contains("avif;"));
    }
}
