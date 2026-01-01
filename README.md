# Entitan — egui example

A minimal Rust binary using `eframe` (egui) to show a button and counter.

Quick start:

- Build and run: `cargo run`

Notes:

- This example uses `eframe` (egui) and `rfd` for file dialogs and is cross-platform. The app requests a minimum window size from the backend when available and also applies a UI-level fallback if unsupported.
- Press the "Increment" button in the app window to increase the counter.
