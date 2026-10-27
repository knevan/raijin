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

- The folder icon opens the resolved destination directory using the existing platform action, with a visible error notification on failure.
- The refresh-circle button resets the folder to the category/default destination.
- The gear is disabled and reserved for future per-download options. No speed/dynamic-part controls are shown until they are wired to the engine.
- `Add paused` adds without starting; the user resumes manually. `Download` adds and starts. The label `Queue` is deferred until the UI offers explicit automatic scheduling behavior rather than implying it through a label change.

No files are moved when a category is selected. During MVP development, the initial migration is edited directly and the local database must be reset when this schema changes.
