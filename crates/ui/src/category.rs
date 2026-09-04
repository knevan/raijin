use raijin_core::download::DownloadStatus;

/// File category inferred from a file name extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileCategory {
    Compressed,
    Programs,
    Videos,
    Music,
    Pictures,
    Documents,
    Other,
}

impl FileCategory {
    /// Short label shown under the file name in the downloads table.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compressed => "Compressed",
            Self::Programs => "Programs",
            Self::Videos => "Videos",
            Self::Music => "Music",
            Self::Pictures => "Pictures",
            Self::Documents => "Documents",
            Self::Other => "Other",
        }
    }

    /// All selectable file categories in sidebar order.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::Compressed,
            Self::Programs,
            Self::Videos,
            Self::Music,
            Self::Pictures,
            Self::Documents,
            Self::Other,
        ]
    }
}

/// Sidebar category filter. `All` disables file-category filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CategoryFilter {
    /// Show every download regardless of file category.
    #[default]
    All,
    /// Show only downloads of one file category.
    Category(FileCategory),
}

impl CategoryFilter {
    /// Label shown in the sidebar and content header.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Category(category) => category.label(),
        }
    }

    /// Returns true when a download of `category` passes this filter.
    #[must_use]
    pub const fn matches(self, category: FileCategory) -> bool {
        match self {
            Self::All => true,
            Self::Category(expected) => expected as u8 == category as u8,
        }
    }
}

/// Sidebar status filter. Finished means fully completed; everything else
/// (active, paused, queued, failed) counts as unfinished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StatusFilter {
    /// Show every download regardless of status.
    #[default]
    All,
    /// Show only completed downloads.
    Finished,
    /// Show only downloads that are not completed.
    Unfinished,
}

impl StatusFilter {
    /// Label shown in the sidebar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Finished => "Finished",
            Self::Unfinished => "Unfinished",
        }
    }

    /// Returns true when a download with `status` passes this filter.
    ///
    /// Removed downloads never pass: the engine deletes them and the monitor
    /// drops them from projections, so reaching this check means a stale row.
    #[must_use]
    pub const fn matches(self, status: DownloadStatus) -> bool {
        match self {
            Self::All => !matches!(status, DownloadStatus::Removed),
            Self::Finished => matches!(status, DownloadStatus::Completed),
            Self::Unfinished => {
                !matches!(status, DownloadStatus::Completed | DownloadStatus::Removed)
            }
        }
    }
}

/// Infers a file category from the file name extension (case-insensitive).
///
/// Files without a recognized extension map to [`FileCategory::Other`].
#[must_use]
pub fn infer_category(file_name: &str) -> FileCategory {
    let extension = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // A leading-dot or extension-less name has no usable extension.
    if !file_name.contains('.') || extension.is_empty() || extension.len() == file_name.len() {
        return FileCategory::Other;
    }
    match extension.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "cab" | "iso" | "jar" => {
            FileCategory::Compressed
        }
        "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" | "apk" => {
            FileCategory::Programs
        }
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts"
        | "m2ts" => FileCategory::Videos,
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" | "mid" | "midi" => {
            FileCategory::Music
        }
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "tiff" | "tif" | "ico"
        | "heic" | "avif" => FileCategory::Pictures,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "csv"
        | "epub" | "mobi" | "odt" | "ods" | "odp" => FileCategory::Documents,
        _ => FileCategory::Other,
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports (see format.rs): a `use super::*` glob would shadow
    // the built-in `#[test]` attribute with GPUI's `test` macro.
    use raijin_core::download::DownloadStatus;

    use super::{CategoryFilter, FileCategory, StatusFilter, infer_category};

    #[test]
    fn inference_should_map_common_extensions() {
        assert_eq!(
            infer_category("ABDownloadManager_1.9.2_windows_x64.exe"),
            FileCategory::Programs
        );
        assert_eq!(
            infer_category("ABDownloadManager_1.9.2_windows_x64.zip"),
            FileCategory::Compressed
        );
        assert_eq!(infer_category("movie.MKV"), FileCategory::Videos);
        assert_eq!(infer_category("song.flac"), FileCategory::Music);
        assert_eq!(infer_category("photo.JPG"), FileCategory::Pictures);
        assert_eq!(infer_category("report.pdf"), FileCategory::Documents);
    }

    #[test]
    fn inference_should_fall_back_to_other() {
        assert_eq!(infer_category("README"), FileCategory::Other);
        assert_eq!(infer_category("archive.unknownext"), FileCategory::Other);
        assert_eq!(infer_category(".gitignore"), FileCategory::Other);
    }

    #[test]
    fn category_filter_should_match_selected_category_only() {
        assert!(CategoryFilter::All.matches(FileCategory::Videos));
        assert!(CategoryFilter::Category(FileCategory::Videos).matches(FileCategory::Videos));
        assert!(!CategoryFilter::Category(FileCategory::Videos).matches(FileCategory::Music));
    }

    #[test]
    fn status_filter_should_split_finished_and_unfinished() {
        assert!(StatusFilter::Finished.matches(DownloadStatus::Completed));
        assert!(!StatusFilter::Finished.matches(DownloadStatus::Downloading));
        assert!(!StatusFilter::Finished.matches(DownloadStatus::Error));
        assert!(StatusFilter::Unfinished.matches(DownloadStatus::Downloading));
        assert!(StatusFilter::Unfinished.matches(DownloadStatus::Paused));
        assert!(StatusFilter::Unfinished.matches(DownloadStatus::Error));
        assert!(!StatusFilter::Unfinished.matches(DownloadStatus::Completed));
        assert!(!StatusFilter::All.matches(DownloadStatus::Removed));
    }
}
