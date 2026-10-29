//! Desktop GUI: pick or drop files, choose a size limit, save the result.

use std::path::{Path, PathBuf};

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;
use docsqueeze::{IMAGE_EXTENSIONS, Report, human};
use futures_channel::mpsc::unbounded;
use futures_util::StreamExt;

const CSS: &str = include_str!("docsqueeze-gui.css");

/// Common portal limits, in the selected unit.
const PRESETS: &[(&str, &str)] =
    &[("200", "KB"), ("500", "KB"), ("1", "MB"), ("2", "MB"), ("5", "MB"), ("10", "MB")];

fn main() {
    let window = WindowBuilder::new()
        .with_title("docsqueeze")
        .with_inner_size(LogicalSize::new(640.0, 780.0))
        .with_min_inner_size(LogicalSize::new(420.0, 520.0));
    LaunchBuilder::desktop().with_cfg(Config::new().with_window(window)).launch(App);
}

#[derive(Clone, PartialEq)]
struct Entry {
    path: PathBuf,
    size: u64,
}

#[derive(Clone)]
enum Phase {
    Idle,
    Running,
    Done(Report),
    Failed(String),
}

enum Msg {
    Log(String),
    Done(Result<Report, String>),
}

#[component]
fn App() -> Element {
    let mut files = use_signal(Vec::<Entry>::new);
    let mut amount = use_signal(|| "2".to_string());
    let mut unit = use_signal(|| "MB".to_string());
    let mut phase = use_signal(|| Phase::Idle);
    let mut log = use_signal(Vec::<String>::new);
    let mut dragging = use_signal(|| false);
    let mut skipped = use_signal(Vec::<String>::new);

    let busy = matches!(*phase.read(), Phase::Running);
    let target = target_bytes(&amount.read(), &unit.read());
    let can_run = !busy && !files.read().is_empty() && target.is_some();

    let pick = move |_| {
        spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Add documents")
                .add_filter("PDFs and images", &accepted_extensions())
                .pick_files()
                .await;
            if let Some(handles) = picked {
                let paths = handles.iter().map(|h| h.path().to_path_buf()).collect();
                skipped.set(add_files(files, paths));
            }
        });
    };

    let squeeze = move |_| {
        let inputs: Vec<PathBuf> = files.read().iter().map(|e| e.path.clone()).collect();
        let Some(target) = target_bytes(&amount.read(), &unit.read()) else {
            return;
        };
        spawn(async move {
            let suggested = match docsqueeze::default_output(&inputs, target) {
                Ok(path) => path,
                Err(e) => return phase.set(Phase::Failed(format!("{e:#}"))),
            };
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Save result as");
            if let Some(dir) = suggested.parent() {
                dialog = dialog.set_directory(dir);
            }
            if let Some(name) = suggested.file_name() {
                dialog = dialog.set_file_name(name.to_string_lossy());
            }
            let Some(handle) = dialog.save_file().await else {
                return;
            };
            let output = handle.path().to_path_buf();
            if inputs.contains(&output) {
                return phase.set(Phase::Failed(
                    "That would overwrite one of your files. Choose a different name.".into(),
                ));
            }

            log.set(Vec::new());
            phase.set(Phase::Running);
            let (tx, mut rx) = unbounded();
            std::thread::spawn(move || {
                let result = docsqueeze::run(&inputs, target, Some(output), &|msg| {
                    let _ = tx.unbounded_send(Msg::Log(msg.to_owned()));
                });
                let _ = tx.unbounded_send(Msg::Done(result.map_err(|e| format!("{e:#}"))));
            });
            while let Some(msg) = rx.next().await {
                match msg {
                    Msg::Log(line) => log.write().push(line),
                    Msg::Done(Ok(report)) => phase.set(Phase::Done(report)),
                    Msg::Done(Err(e)) => phase.set(Phase::Failed(e)),
                }
            }
        });
    };

    let count = files.read().len();
    let total: u64 = files.read().iter().map(|e| e.size).sum();
    let plan = match files.read().as_slice() {
        [] => String::new(),
        [one] if is_pdf(&one.path) => "Shrinks the PDF.".into(),
        [_] => "Saves as JPEG. Name the file .pdf to get a PDF instead.".into(),
        many => format!("Merges {} files into one PDF, in this order.", many.len()),
    };

    rsx! {
        style { {CSS} }
        main {
            class: if dragging() { "app dragging" } else { "app" },
            ondragover: move |e| {
                e.prevent_default();
                dragging.set(true);
            },
            ondragleave: move |_| dragging.set(false),
            ondrop: move |e| {
                e.prevent_default();
                dragging.set(false);
                if !busy {
                    let paths = e.data_transfer().files().iter().map(|f| f.path()).collect();
                    skipped.set(add_files(files, paths));
                }
            },

            header {
                h1 { "docsqueeze" }
                p { class: "muted", "Make documents fit upload limits. Files stay on this computer." }
            }

            section { class: "card",
                div { class: "row between",
                    h2 { "Files" }
                    if count > 0 {
                        span { class: "muted", "{count} · {human(total as usize)}" }
                    }
                }
                if count == 0 {
                    button { class: "dropzone", disabled: busy, onclick: pick,
                        strong { "Drop PDFs or images here" }
                        span { class: "muted", "or click to choose files" }
                    }
                } else {
                    ol { class: "files",
                        for (i, entry) in files.read().iter().enumerate() {
                            li { key: "{entry.path.display()}",
                                span { class: "badge", {file_kind(&entry.path)} }
                                span { class: "name", title: "{entry.path.display()}", {file_name(&entry.path)} }
                                span { class: "muted size", {human(entry.size as usize)} }
                                button {
                                    class: "icon", title: "Move up", disabled: busy || i == 0,
                                    onclick: move |_| files.write().swap(i, i - 1),
                                    "↑"
                                }
                                button {
                                    class: "icon", title: "Move down", disabled: busy || i + 1 == count,
                                    onclick: move |_| files.write().swap(i, i + 1),
                                    "↓"
                                }
                                button {
                                    class: "icon", title: "Remove", disabled: busy,
                                    onclick: move |_| { files.write().remove(i); },
                                    "✕"
                                }
                            }
                        }
                    }
                    div { class: "row",
                        button { class: "secondary", disabled: busy, onclick: pick, "Add files" }
                        button {
                            class: "secondary", disabled: busy,
                            onclick: move |_| files.write().clear(),
                            "Clear"
                        }
                    }
                    p { class: "muted", "{plan}" }
                }
                if !skipped.read().is_empty() {
                    p { class: "warn", "Skipped (not a PDF or image): {skipped.read().join(\", \")}" }
                }
            }

            section { class: "card",
                h2 { "Size limit" }
                div { class: "row",
                    input {
                        r#type: "number", min: "0", step: "any", inputmode: "decimal",
                        value: "{amount}", disabled: busy,
                        oninput: move |e| amount.set(e.value()),
                    }
                    select {
                        value: "{unit}", disabled: busy,
                        onchange: move |e| unit.set(e.value()),
                        option { value: "KB", "KB" }
                        option { value: "MB", "MB" }
                    }
                }
                div { class: "row presets",
                    for &(value, preset_unit) in PRESETS {
                        button {
                            class: if *amount.read() == value && *unit.read() == preset_unit { "chip active" } else { "chip" },
                            disabled: busy,
                            onclick: move |_| {
                                amount.set(value.to_string());
                                unit.set(preset_unit.to_string());
                            },
                            "{value} {preset_unit}"
                        }
                    }
                }
                if target.is_none() {
                    p { class: "warn", "Enter a size greater than 0." }
                }
            }

            button { class: "primary", disabled: !can_run, onclick: squeeze,
                if busy { "Squeezing…" } else { "Squeeze and save…" }
            }

            {match &*phase.read() {
                Phase::Idle => rsx! {},
                Phase::Running => rsx! {
                    section { class: "card status", p { "Working… large scans can take a minute." } }
                },
                Phase::Done(report) => {
                    let output = report.output.clone();
                    rsx! {
                        section { class: if report.met_target() { "card status ok" } else { "card status warn-card" },
                            if report.met_target() {
                                h2 { "Done: {human(report.size)}" }
                            } else {
                                h2 { "Could not reach {human(report.target)}" }
                                p { "Saved the smallest possible version ({human(report.size)}). Try fewer pages or a higher limit." }
                            }
                            p { class: "muted", "{report.reduction():.0}% smaller than the original {human(report.original_size)}." }
                            div { class: "row",
                                span { class: "name", title: "{output.display()}", {file_name(&output)} }
                                button { class: "secondary", onclick: move |_| reveal(&output), "Show in folder" }
                            }
                        }
                    }
                }
                Phase::Failed(error) => rsx! {
                    section { class: "card status error", h2 { "Something went wrong" } p { "{error}" } }
                },
            }}

            if !log.read().is_empty() {
                details { class: "log",
                    summary { "Details" }
                    pre { {log.read().join("\n")} }
                }
            }
        }
    }
}

/// Append supported files that are not already listed; returns names of the rest.
fn add_files(mut files: Signal<Vec<Entry>>, paths: Vec<PathBuf>) -> Vec<String> {
    let mut skipped = Vec::new();
    let mut list = files.write();
    for path in paths {
        let supported = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| accepted_extensions().iter().any(|a| a.eq_ignore_ascii_case(e)));
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !supported || !meta.is_file() {
            skipped.push(file_name(&path));
        } else if !list.iter().any(|e| e.path == path) {
            list.push(Entry { path, size: meta.len() });
        }
    }
    skipped
}

fn accepted_extensions() -> Vec<&'static str> {
    std::iter::once("pdf").chain(IMAGE_EXTENSIONS.iter().copied()).collect()
}

fn target_bytes(amount: &str, unit: &str) -> Option<usize> {
    let value: f64 = amount.trim().parse().ok()?;
    let scale = if unit == "KB" { 1_000.0 } else { 1_000_000.0 };
    let bytes = (value * scale) as usize;
    (value.is_finite() && bytes > 0).then_some(bytes)
}

fn is_pdf(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

fn file_kind(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default()
}

/// Open the system file manager with `path` selected where supported.
fn reveal(path: &Path) {
    use std::process::Command;
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()
    };
    if let Err(e) = result {
        eprintln!("cannot open file manager: {e}");
    }
}
