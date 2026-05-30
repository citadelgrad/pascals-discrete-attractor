use leptos::prelude::*;

#[allow(unused_imports)]
use crate::components::markdown_render::render_markdown;

use crate::server::projects::get_cached_documents;

#[cfg(feature = "hydrate")]
use serde::Deserialize;

/// SSE document update event
#[cfg(feature = "hydrate")]
#[derive(Deserialize, Clone, Debug)]
struct DocumentUpdate {
    doc_type: String,
    #[serde(default)]
    content: Option<String>,
}

/// Active tab in the document viewer (used in narrow/tabbed mode)
#[derive(Clone, Copy, PartialEq)]
enum DocTab {
    Prd,
    Spec,
}

/// Document viewer that subscribes to SSE at `/api/documents/stream?project_id=<id>`
/// for live updates as Claude Code writes PRD/Spec files.
///
/// Loads cached documents from SQLite on mount for instant display, then connects
/// to SSE for live updates.
///
/// At narrow widths, PRD and Spec are shown as tabs in a single panel.
/// At wide widths (fullscreen), each gets its own column — the parent
/// layout switches to 3-column mode via CSS.
#[component]
pub fn DocumentViewer<FP, FS>(
    project_id: i64,
    on_prd_change: FP,
    on_spec_change: FS,
) -> impl IntoView
where
    FP: Fn(bool) + Copy + Send + Sync + 'static,
    FS: Fn(bool) + Copy + Send + Sync + 'static,
{
    let (active_tab, set_active_tab) = signal(DocTab::Prd);
    let (prd_content, set_prd_content) = signal(String::new());
    let (spec_content, set_spec_content) = signal(String::new());

    // Load cached documents from SQLite on mount
    let cached = Resource::new(move || project_id, get_cached_documents);

    // Update signals when cached data loads
    Effect::new(move || {
        if let Some(Ok(docs)) = cached.get() {
            if let Some(prd) = docs.prd {
                set_prd_content.set(prd.clone());
                on_prd_change(!prd.is_empty());
            }
            if let Some(spec) = docs.spec {
                set_spec_content.set(spec.clone());
                on_spec_change(!spec.is_empty());
            }
        }
    });

    // Subscribe to document updates via SSE
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move || {
            leptos::task::spawn_local(async move {
                use futures::StreamExt as _;

                let url = format!("/api/documents/stream?project_id={}", project_id);
                match gloo_net::eventsource::futures::EventSource::new(&url) {
                    Ok(mut es) => {
                        let mut stream = es.subscribe("document_update").unwrap();

                        while let Some(Ok((_, msg))) = stream.next().await {
                            if let Some(data_str) = msg.data().as_string() {
                                if let Ok(update) =
                                    serde_json::from_str::<DocumentUpdate>(&data_str)
                                {
                                    let content = update.content.unwrap_or_default();
                                    let has_content = !content.is_empty();

                                    match update.doc_type.as_str() {
                                        "prd" => {
                                            set_prd_content.set(content);
                                            on_prd_change(has_content);
                                        }
                                        "spec" => {
                                            set_spec_content.set(content);
                                            on_spec_change(has_content);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        es.close();
                    }
                    Err(e) => {
                        tracing::error!("Failed to connect to document SSE: {:?}", e);
                    }
                }
            });
        });
    }

    let prd_html = move || render_markdown(&prd_content.get());
    let spec_html = move || render_markdown(&spec_content.get());

    view! {
        // Tabbed view for narrow widths — hidden at wide breakpoint
        <div class="document-viewer doc-tabbed">
            <div class="doc-tabs">
                <button
                    class=move || if active_tab.get() == DocTab::Prd { "doc-tab active" } else { "doc-tab" }
                    on:click=move |_| set_active_tab.set(DocTab::Prd)
                >
                    "PRD"
                </button>
                <button
                    class=move || if active_tab.get() == DocTab::Spec { "doc-tab active" } else { "doc-tab" }
                    on:click=move |_| set_active_tab.set(DocTab::Spec)
                >
                    "Spec"
                </button>
            </div>

            <div class="doc-content">
                {move || match active_tab.get() {
                    DocTab::Prd => {
                        let html = prd_html();
                        if html.is_empty() {
                            view! {
                                <div class="doc-placeholder">
                                    <p>"No PRD yet. Use Claude Code to create one:"</p>
                                    <code>"\"Create a PRD for ...\""</code>
                                </div>
                            }.into_any()
                        } else {
                            view! {
                                <div class="markdown-rendered" inner_html=html></div>
                            }.into_any()
                        }
                    }
                    DocTab::Spec => {
                        let html = spec_html();
                        if html.is_empty() {
                            view! {
                                <div class="doc-placeholder">
                                    <p>"No Spec yet. Use Claude Code to create one:"</p>
                                    <code>"\"Now create the technical spec\""</code>
                                </div>
                            }.into_any()
                        } else {
                            view! {
                                <div class="markdown-rendered" inner_html=html></div>
                            }.into_any()
                        }
                    }
                }}
            </div>
        </div>

        // PRD column — visible only at wide breakpoint
        <div class="doc-column doc-column-prd">
            <div class="doc-column-header">"PRD"</div>
            <div class="doc-content">
                {move || {
                    let html = prd_html();
                    if html.is_empty() {
                        view! {
                            <div class="doc-placeholder">
                                <p>"No PRD yet."</p>
                                <code>"\"Create a PRD for ...\""</code>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="markdown-rendered" inner_html=html></div>
                        }.into_any()
                    }
                }}
            </div>
        </div>

        // Spec column — visible only at wide breakpoint
        <div class="doc-column doc-column-spec">
            <div class="doc-column-header">"Spec"</div>
            <div class="doc-content">
                {move || {
                    let html = spec_html();
                    if html.is_empty() {
                        view! {
                            <div class="doc-placeholder">
                                <p>"No Spec yet."</p>
                                <code>"\"Now create the technical spec\""</code>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="markdown-rendered" inner_html=html></div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}
