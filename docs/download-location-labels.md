# Download categories and table subtitles

## Current decision

A category is user-selected grouping metadata, independent of destination folder and file extension. This supersedes the earlier folder-derived location label rule.

- Checked `Use Category`: persist the selected dropdown label, even when the user enters a custom destination folder.
- Unchecked `Use Category`: persist no category (`NULL`); display and filter the item as `General`, even inside a known category folder.
- Table subtitles and sidebar category filters use this persisted category. File icons continue to use the file extension.
- Queue names never replace the subtitle, including the default `Main` queue. Queue execution and membership remain unchanged.
- Category selection suggests `<category_root>/<label>` only until the user manually edits the folder. Changing the category or checkbox must not overwrite a custom folder. Reset explicitly restores the category/default destination.

The optional `downloads.category` field passes through `NewDownload`, `DownloadItem`, repositories, and `DownloadView`. Category values must be nonempty, trimmed labels of at most 128 characters. Headless/API additions without a desktop category selection default to `General`.

Rejected: deriving the category from the destination folder or file extension. Both prevent a user from grouping files independently of storage layout. A program may belong to `Compressed` while retaining its program file icon. Custom category creation is deferred; no category editor, category table, or new queue UI is introduced now.

## New Download actions

- The folder icon opens GPUI's native single-directory picker regardless of `Use Category` or whether the current destination exists. Selecting a directory replaces the path and marks it as manual without changing the selected category or checkbox. Cancel leaves the path unchanged. Picker failures show a notification; missing folders are not created implicitly.
- Open both manual and integration confirmation windows through `gpui_kit::open_window`, which installs the Base `Root` and component notification state. A bare `App::open_window` leaves that state missing; reporting an inaccessible folder then panics in `push_notification`, causing Windows fail-fast. Headless UI regression coverage exercises the picker from a missing destination, selection and cancellation in both checkbox states, custom-path preservation, reset, and notification support.
- The refresh-circle button resets the folder to the category/default destination.
- The gear is disabled and reserved for future per-download options. No speed/dynamic-part controls are shown until they are wired to the engine.
- `Add paused` adds without starting; the user resumes manually. `Download` adds and starts. The label `Queue` is deferred until the UI offers explicit automatic scheduling behavior rather than implying it through a label change.

No files are moved when a category is selected. During MVP development, the initial migration is edited directly and the local database must be reset when this schema changes.
