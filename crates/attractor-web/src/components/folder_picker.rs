use crate::server::projects::{list_directory, open_project, DirEntry, Project};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;

/// FolderPicker component for selecting a project folder.
///
/// Provides two input methods:
/// 1. Text input for pasting/typing an absolute path
/// 2. Directory browser for navigating the server filesystem
///
/// ## Props
/// - `on_select`: Called with the opened Project when a folder is selected
/// - `on_close`: Called to dismiss the modal
#[component]
pub fn FolderPicker<F>(on_select: F, on_close: impl Fn() + Clone + 'static) -> impl IntoView
where
    F: Fn(Project) + Clone + Send + Sync + 'static,
{
    let on_close_clone = on_close.clone();

    // Text input state
    let (path_input, set_path_input) = signal(String::new());
    let (input_error, set_input_error) = signal(String::new());
    let (input_loading, set_input_loading) = signal(false);

    // Directory browser state
    let (current_path, set_current_path) = signal(String::new());
    let (dir_entries, set_dir_entries) = signal(Vec::<DirEntry>::new());
    let (browser_loading, set_browser_loading) = signal(false);
    let (browser_error, set_browser_error) = signal(String::new());

    // Load home directory on mount
    Effect::new(move |_| {
        set_browser_loading.set(true);
        set_browser_error.set(String::new());

        spawn_local({
            async move {
                match list_directory("".to_string()).await {
                    Ok(entries) => {
                        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
                        set_current_path.set(home);
                        set_dir_entries.set(entries);
                    }
                    Err(e) => {
                        set_browser_error.set(format!("Failed to load directory: {}", e));
                    }
                }
                set_browser_loading.set(false);
            }
        });
    });

    // Handle text input open
    let handle_input_open = {
        let on_select = on_select.clone();
        let on_close = on_close.clone();
        move |_| {
            let path = path_input.get();
            if path.is_empty() {
                set_input_error.set("Please enter a path".to_string());
                return;
            }

            set_input_loading.set(true);
            set_input_error.set(String::new());

            spawn_local({
                let path = path.clone();
                let on_select = on_select.clone();
                let on_close = on_close.clone();
                async move {
                    match open_project(path).await {
                        Ok(project) => {
                            on_select(project);
                            on_close();
                        }
                        Err(e) => {
                            set_input_error.set(e.to_string());
                        }
                    }
                    set_input_loading.set(false);
                }
            });
        }
    };

    // Handle directory navigation
    let handle_nav = {
        move |path: String| {
            set_browser_loading.set(true);
            set_browser_error.set(String::new());

            spawn_local({
                async move {
                    match list_directory(path.clone()).await {
                        Ok(entries) => {
                            set_current_path.set(path);
                            set_dir_entries.set(entries);
                        }
                        Err(e) => {
                            set_browser_error.set(format!("Failed to load directory: {}", e));
                        }
                    }
                    set_browser_loading.set(false);
                }
            });
        }
    };

    // Handle folder selection via browser
    let handle_browser_select = {
        let on_select = on_select.clone();
        let on_close = on_close.clone();
        move |_| {
            let path = current_path.get();
            set_browser_loading.set(true);
            set_browser_error.set(String::new());

            spawn_local({
                let on_select = on_select.clone();
                let on_close = on_close.clone();
                async move {
                    match open_project(path).await {
                        Ok(project) => {
                            on_select(project);
                            on_close();
                        }
                        Err(e) => {
                            set_browser_error.set(e.to_string());
                        }
                    }
                    set_browser_loading.set(false);
                }
            });
        }
    };

    // Handle escape key
    let handle_keydown = {
        let on_close = on_close_clone;
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Escape" {
                on_close();
            }
        }
    };

    let on_close_overlay = on_close.clone();
    let on_close_btn = on_close;

    view! {
        <div class="folder-picker-overlay" on:click={move |_| on_close_overlay()}>
            <div
                class="folder-picker-modal"
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                on:keydown=handle_keydown
            >
                <div class="folder-picker-header">
                    <h2>Open Project</h2>
                    <button
                        class="close-btn"
                        on:click=move |_| on_close_btn()
                    >
                        {"\u{00D7}"}
                    </button>
                </div>

                <div class="folder-picker-content">
                    {/* Text Input Section */}
                    <div class="input-section">
                        <h3>Direct Path</h3>
                        <input
                            type="text"
                            class="path-input"
                            placeholder="/Users/you/projects/my-app"
                            value=path_input
                            on:input=move |ev| set_path_input.set(event_target_value(&ev))
                            disabled=input_loading
                        />
                        <button
                            class="open-btn"
                            on:click=handle_input_open
                            disabled=input_loading
                        >
                            {move || if input_loading.get() { "Opening..." } else { "Open" }}
                        </button>
                        {move || {
                            if !input_error.get().is_empty() {
                                view! {
                                    <div class="error-message">{move || input_error.get()}</div>
                                }.into_any()
                            } else {
                                view! { <span></span> }.into_any()
                            }
                        }}
                    </div>

                    <div class="divider">OR</div>

                    {/* Directory Browser Section */}
                    <div class="browser-section">
                        <h3>Browse Directories</h3>
                        <div class="breadcrumb">
                            <span class="breadcrumb-path">{move || current_path.get()}</span>
                        </div>

                        <div class="directory-list">
                            {move || {
                                if browser_loading.get() {
                                    view! {
                                        <div class="loading-indicator">"Loading..."</div>
                                    }.into_any()
                                } else if !browser_error.get().is_empty() {
                                    view! {
                                        <div class="error-message">{move || browser_error.get()}</div>
                                    }.into_any()
                                } else {
                                    let on_nav = handle_nav;
                                    view! {
                                        <For
                                            each=move || dir_entries.get()
                                            key=|entry| entry.path.clone()
                                            children=move |entry: DirEntry| {
                                                let path = entry.path.clone();
                                                let on_nav_click = on_nav;
                                                view! {
                                                    <div
                                                        class="dir-entry"
                                                        on:click=move |_| on_nav_click(path.clone())
                                                    >
                                                        <span class="dir-icon">{"\u{1F4C1}"}</span>
                                                        <span class="dir-name">{entry.name}</span>
                                                    </div>
                                                }
                                            }
                                        />
                                    }.into_any()
                                }
                            }}
                        </div>

                        <button
                            class="select-btn"
                            on:click=handle_browser_select
                            disabled=browser_loading
                        >
                            {move || if browser_loading.get() { "Opening..." } else { "Select This Folder" }}
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}
