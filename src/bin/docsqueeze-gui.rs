//! Desktop GUI: pick or drop files, choose a size limit, then either combine
//! them into one PDF or shrink each one separately.

// No console window behind the GUI on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashMap;
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
        .with_title("Docsqueeze")
        .with_inner_size(LogicalSize::new(640.0, 820.0))
        .with_min_inner_size(LogicalSize::new(420.0, 520.0));
    LaunchBuilder::desktop().with_cfg(Config::new().with_window(window)).launch(App);
}

#[derive(Clone, PartialEq)]
struct Entry {
    path: PathBuf,
    size: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Merge all files into one PDF.
    Combine,
    /// Shrink every file on its own.
    Batch,
}

#[derive(Clone)]
enum Phase {
    Idle,
    /// Batch progress as (finished, total); `None` when combining.
    Running(Option<(usize, usize)>),
    Done(Report),
    BatchDone(PathBuf),
    Failed(String),
}

type Outcome = Result<Report, String>;

enum Msg {
    Log(String),
    Done(Outcome),
    Item(PathBuf, Outcome),
}

#[component]
fn App() -> Element {
    let mut mode = use_signal(|| Mode::Combine);
    let mut files = use_signal(Vec::<Entry>::new);
    let mut amount = use_signal(|| "2".to_string());
    let mut unit = use_signal(|| "MB".to_string());
    let mut as_pdf = use_signal(|| false);
    let mut phase = use_signal(|| Phase::Idle);
    let mut results = use_signal(HashMap::<PathBuf, Outcome>::new);
    let mut log = use_signal(Vec::<String>::new);
    let mut dragging = use_signal(|| false);
    let mut skipped = use_signal(Vec::<String>::new);

    let busy = matches!(*phase.read(), Phase::Running(_));
    let target = target_bytes(&amount.read(), &unit.read());
    let can_run = !busy && !files.read().is_empty() && target.is_some();
    let combine = mode() == Mode::Combine;

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

    let run_combine = move || {
        let inputs: Vec<PathBuf> = files.read().iter().map(|e| e.path.clone()).collect();
        let Some(target) = target_bytes(&amount.read(), &unit.read()) else {
            return;
        };
        spawn(async move {
            let mut suggested = match docsqueeze::default_output(&inputs, target) {
                Ok(path) => path,
                Err(e) => return phase.set(Phase::Failed(format!("{e:#}"))),
            };
            // Combining always yields a PDF, even from a single image.
            suggested.set_extension("pdf");
            let Some(output) = save_dialog(&suggested).await else {
                return;
            };
            if inputs.contains(&output) {
                return phase.set(Phase::Failed(
                    "That would overwrite one of your files. Choose a different name.".into(),
                ));
            }

            log.set(Vec::new());
            phase.set(Phase::Running(None));
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
                    Msg::Item(..) => {}
                }
            }
        });
    };

    let run_batch = move || {
        let inputs: Vec<PathBuf> = files.read().iter().map(|e| e.path.clone()).collect();
        let Some(target) = target_bytes(&amount.read(), &unit.read()) else {
            return;
        };
        let as_pdf = as_pdf();
        spawn(async move {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose a folder for the results");
            if let Some(dir) = inputs[0].parent() {
                dialog = dialog.set_directory(dir);
            }
            let Some(folder) = dialog.pick_folder().await else {
                return;
            };
            let folder = folder.path().to_path_buf();
            let outputs = match docsqueeze::batch_outputs(&inputs, target, Some(&folder), as_pdf) {
                Ok(outputs) => outputs,
                Err(e) => return phase.set(Phase::Failed(format!("{e:#}"))),
            };

            let total = inputs.len();
            log.set(Vec::new());
            results.write().clear();
            phase.set(Phase::Running(Some((0, total))));
            let (tx, mut rx) = unbounded();
            std::thread::spawn(move || {
                for (input, output) in inputs.into_iter().zip(outputs) {
                    let result =
                        docsqueeze::run(std::slice::from_ref(&input), target, Some(output), &|msg| {
                            let _ = tx.unbounded_send(Msg::Log(msg.to_owned()));
                        });
                    let _ = tx.unbounded_send(Msg::Item(input, result.map_err(|e| format!("{e:#}"))));
                }
            });
            let mut finished = 0;
            while let Some(msg) = rx.next().await {
                match msg {
                    Msg::Log(line) => log.write().push(line),
                    Msg::Item(input, outcome) => {
                        results.write().insert(input, outcome);
                        finished += 1;
                        phase.set(Phase::Running(Some((finished, total))));
                    }
                    Msg::Done(_) => {}
                }
            }
            phase.set(Phase::BatchDone(folder));
        });
    };

    let count = files.read().len();
    let total: u64 = files.read().iter().map(|e| e.size).sum();
    let plan = match (mode(), count) {
        (_, 0) => String::new(),
        (Mode::Combine, 1) => "Saves the file as a PDF.".into(),
        (Mode::Combine, n) => format!("Combines {n} files into one PDF, in this order."),
        (Mode::Batch, _) if as_pdf() => "Shrinks each file separately and saves each as a PDF.".into(),
        (Mode::Batch, _) => "Shrinks each file separately. PDFs stay PDFs; images are saved as JPEG.".into(),
    };
    let limit = target.map(human).unwrap_or_default();

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

            nav { class: "tabs", role: "tablist",
                for (tab, label) in [(Mode::Combine, "Combine into one PDF"), (Mode::Batch, "Shrink each file")] {
                    button {
                        role: "tab",
                        class: if mode() == tab { "tab active" } else { "tab" },
                        aria_selected: mode() == tab,
                        disabled: busy,
                        onclick: move |_| {
                            if mode() != tab {
                                mode.set(tab);
                                phase.set(Phase::Idle);
                            }
                        },
                        "{label}"
                    }
                }
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
                                if combine {
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
                                } else {
                                    {batch_status(entry, results.read().get(&entry.path))}
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
                            onclick: move |_| {
                                files.write().clear();
                                results.write().clear();
                            },
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
                h2 { if combine { "Size limit" } else { "Size limit per file" } }
                div { class: "row",
                    input {
                        r#type: "number", min: "0", step: "any", inputmode: "decimal",
                        value: "{amount}", disabled: busy,
                        oninput: move |e| amount.set(e.value()),
                    }
                    // `selected` on the options, since a select's `value` is
                    // applied before its options exist.
                    select {
                        disabled: busy,
                        onchange: move |e| unit.set(e.value()),
                        option { value: "KB", selected: *unit.read() == "KB", "KB" }
                        option { value: "MB", selected: *unit.read() == "MB", "MB" }
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
                if !combine {
                    label { class: "check",
                        input {
                            r#type: "checkbox", checked: as_pdf(), disabled: busy,
                            onchange: move |e| as_pdf.set(e.checked()),
                        }
                        "Save images as PDF"
                    }
                }
            }

            button {
                class: "primary", disabled: !can_run,
                onclick: move |_| if combine { run_combine() } else { run_batch() },
                match (busy, combine) {
                    (true, _) => "Working…".to_string(),
                    (false, true) => "Combine and save…".to_string(),
                    (false, false) if count == 1 => "Shrink 1 file…".to_string(),
                    (false, false) => format!("Shrink {count} files…"),
                }
            }

            {match &*phase.read() {
                Phase::Idle => rsx! {},
                Phase::Running(progress) => {
                    let text = match progress {
                        Some((finished, total)) => format!("Working… {finished} of {total} done."),
                        None => "Working… large scans can take a minute.".to_string(),
                    };
                    rsx! { section { class: "card status", p { "{text}" } } }
                }
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
                Phase::BatchDone(folder) => {
                    let results = results.read();
                    let saved: Vec<&Report> = results.values().filter_map(|r| r.as_ref().ok()).collect();
                    let over = saved.iter().filter(|r| !r.met_target()).count();
                    let failed = results.len() - saved.len();
                    let size: usize = saved.iter().map(|r| r.size).sum();
                    let original: usize = saved.iter().map(|r| r.original_size).sum();
                    let folder = folder.clone();
                    let class = match (failed, over) {
                        (0, 0) => "card status ok",
                        _ if saved.is_empty() => "card status error",
                        _ => "card status warn-card",
                    };
                    rsx! {
                        section { class,
                            h2 { "Saved {saved.len()} of {results.len()} files" }
                            if !saved.is_empty() {
                                p { class: "muted",
                                    "{human(size)} in total, {100.0 * (1.0 - size as f64 / original as f64):.0}% smaller."
                                }
                            }
                            if over > 0 {
                                p { "{over} could not reach {limit}; the smallest possible version was saved." }
                            }
                            if failed > 0 {
                                p { "{failed} failed. Hover over a file for details." }
                            }
                            div { class: "row",
                                span { class: "name", title: "{folder.display()}", {file_name(&folder)} }
                                button { class: "secondary", onclick: move |_| open(&folder), "Open folder" }
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

/// Size column for a file in batch mode: before, or before → after.
fn batch_status(entry: &Entry, outcome: Option<&Outcome>) -> Element {
    let before = human(entry.size as usize);
    match outcome {
        None => rsx! { span { class: "muted size", "{before}" } },
        Some(Ok(report)) => {
            let class = if report.met_target() { "size result ok" } else { "size result over" };
            let title = format!("Saved as {}", report.output.display());
            rsx! { span { class, title, "{before} → {human(report.size)}" } }
        }
        Some(Err(error)) => rsx! { span { class: "size result failed", title: "{error}", "Failed" } },
    }
}

async fn save_dialog(suggested: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Save result as");
    if let Some(dir) = suggested.parent() {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = suggested.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    Some(dialog.save_file().await?.path().to_path_buf())
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

/// Open a folder in the system file manager.
fn open(folder: &Path) {
    use std::process::Command;
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = Command::new(program).arg(folder).spawn() {
        eprintln!("cannot open file manager: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_bytes_parses_amounts() {
        assert_eq!(target_bytes("2", "MB"), Some(2_000_000));
        assert_eq!(target_bytes(" 0.5 ", "MB"), Some(500_000));
        assert_eq!(target_bytes("200", "KB"), Some(200_000));
        for bad in ["", "0", "-1", "abc", "inf", "NaN", "0.0000001"] {
            assert_eq!(target_bytes(bad, "MB"), None, "{bad:?}");
        }
    }

    #[test]
    fn presets_are_valid_targets() {
        for (amount, unit) in PRESETS {
            assert!(target_bytes(amount, unit).is_some(), "{amount} {unit}");
        }
    }

    #[test]
    fn accepts_pdf_and_every_image_extension() {
        let accepted = accepted_extensions();
        assert!(accepted.contains(&"pdf"));
        assert!(IMAGE_EXTENSIONS.iter().all(|e| accepted.contains(e)));
    }

    #[test]
    fn file_labels() {
        let path = Path::new("dir").join("scan.final.pdf");
        assert_eq!(file_name(&path), "scan.final.pdf");
        assert_eq!(file_kind(&path), "PDF");
        assert_eq!(file_kind(Path::new("noext")), "");
    }
}
