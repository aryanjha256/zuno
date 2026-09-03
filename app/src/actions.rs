//! Zuno's application-level verbs. See architecture.md §5.
//!
//! Text-editing actions live in `input::text_input` under the `text_input`
//! namespace, scoped to the `TextInput` key context, so they can't fire when a
//! table or the response pane holds focus.
//!
//! Note the name `SendRequest` rather than `Send`: an action named `Send` would
//! shadow `std::marker::Send` at every `use` site, which breaks generic bounds in
//! confusing ways.

use gpui::actions;

actions!(
    zuno,
    [
        // Focus movement
        FocusUrl,
        FocusBody,
        FocusResponse,
        FocusNext,
        FocusPrev,
        // Navigation
        OpenRequest,
        SwitchEnvironment,
        ShowHistory,
        OpenPalette,
        OpenAppMenu,
        PickerNext,
        PickerPrev,
        PickerConfirm,
        PickerDismiss,
        // The collection panel. `Ctrl+P` is a *finder* — it needs you to know the name of the
        // thing you want. The panel is the *browser*, and until it existed nothing in Zuno could
        // answer "what have I saved", since `collection::scan` had exactly one caller.
        ToggleCollectionPanel,
        CollectionNext,
        CollectionPrev,
        CollectionConfirm,
        CollectionCollapse,
        CollectionExpand,
        // Whole-tree folding. `CollectionCollapse` acts on the selected directory; these act on
        // everything, which is a *new fold path* — and `rebuild_tree_visible` warns that one has
        // to move the selection itself, since the panel has no clamp.
        CollectionCollapseAll,
        CollectionExpandAll,
        OpenCollectionMenu,
        // Two actions, because deleting a file has no undo: `DeleteRequest` only *asks*, and
        // `ConfirmDeleteRequest` is the one that removes anything. Splitting them is what lets
        // the confirmation be an ordinary menu row rather than a modal of its own.
        DeleteRequest,
        ConfirmDeleteRequest,
        TrashRequest,
        DuplicateRequest,
        RevealRequest,
        OpenRequestExternally,
        CopyRequestPath,
        CopyRequestRelativePath,
        // Rename is inline in the tree rather than a modal, so it needs a start, a commit and
        // a cancel — the three states an editable row has.
        RenameRequest,
        CommitRename,
        CancelRename,
        // Organising, as distinct from editing. Until these existed every `Ctrl+S` landed flat
        // at the collection root and nothing could put a request anywhere else.
        NewFolder,
        MoveRequest,
        // Import a whole API at once. `ImportCurl` is one request per paste; this is the way a
        // collection gets filled from what a team already has.
        ImportOpenApi,
        ImportConfirm,
        ImportDismiss,
        // Buffers
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        // Request editing
        OpenMethod,
        AddHeader,
        AddQuery,
        ToggleRow,
        RemoveRow,
        OpenBodyType,
        AddFormField,
        ChooseBodyFile,
        AddMultipartField,
        ImportCurl,
        // Request sections. Three tabs, so unlike the response pane's two these cannot be
        // served by one cycling action: clicking Body from Headers is two steps, not one.
        NextRequestTab,
        PrevRequestTab,
        ShowHeadersTab,
        ShowParamsTab,
        ShowBodyTab,
        // Response viewer
        ToggleResponseView,
        FindInResponse,
        FindNext,
        FindPrev,
        CloseFind,
        // The request body's own bar. Separate actions rather than shared ones, because both
        // bars can be open at once and a shared `enter` would have to guess which is meant.
        FindInBody,
        BodyFindNext,
        BodyFindPrev,
        CloseBodyFind,
        ReplaceNext,
        ReplaceAll,
        FoldAll,
        UnfoldAll,
        ResponseRowNext,
        ResponseRowPrev,
        ScrollLeft,
        ScrollRight,
        ScrollStart,
        ToggleFold,
        CopyRowValue,
        CopyRowPath,
        OpenRowMenu,
        MenuNext,
        MenuPrev,
        MenuConfirm,
        MenuDismiss,
        CopyResponse,
        SaveResponse,
        CopyAsCurl,
        // Settings
        OpenSettings,
        SettingNext,
        SettingPrev,
        SettingIncrease,
        SettingDecrease,
        SettingConfirm,
        SettingsDismiss,
        ClearCookies,
        // Request lifecycle
        SaveRequest,
        SendRequest,
        CancelRequest,
        // Application
        ToggleTheme,
        Quit,
    ]
);
