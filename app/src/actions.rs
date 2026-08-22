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
        PickerNext,
        PickerPrev,
        PickerConfirm,
        PickerDismiss,
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
        // Response viewer
        ToggleResponseView,
        FoldAll,
        UnfoldAll,
        CopyResponse,
        SaveResponse,
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
