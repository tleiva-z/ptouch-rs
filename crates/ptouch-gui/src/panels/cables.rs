// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Cable flag and wrap generator.
//!
//! Fills the existing element list so the canvas preview and the Print button
//! keep working unchanged.

use std::path::Path;

use log::{error, info};

use ptouch_render::cable::{self, CableStyle};
use ptouch_render::text::TextRenderer;

use crate::state::{AppState, CableKind, CableSource};

/// Render the cable-label section of the sidebar.
pub fn show_cables(ui: &mut egui::Ui, state: &mut AppState) {
    ui.heading("Cables");
    ui.add_space(4.0);
    ui.label("Bandera Brother: cinta de 12 mm. Envolvente: 18 o 24 mm.");
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.selectable_value(&mut state.cable.kind, CableKind::Flag, "Bandera");
        ui.selectable_value(&mut state.cable.kind, CableKind::Wrap, "Envolvente");
    });

    ui.horizontal(|ui| {
        ui.label("Diámetro");
        ui.add(
            egui::DragValue::new(&mut state.cable.diameter_mm)
                .speed(0.1)
                .range(0.5..=80.0)
                .suffix(" mm"),
        );
    });

    match state.cable.kind {
        CableKind::Flag => {
            ui.horizontal(|ui| {
                ui.label("Holgura");
                ui.add(
                    egui::DragValue::new(&mut state.cable.slack_mm)
                        .speed(0.1)
                        .range(0.0..=30.0)
                        .suffix(" mm"),
                );
            });
        }
        CableKind::Wrap => {
            ui.horizontal(|ui| {
                ui.label("Solape");
                ui.add(
                    egui::DragValue::new(&mut state.cable.overlap_mm)
                        .speed(0.1)
                        .range(0.0..=40.0)
                        .suffix(" mm"),
                );
            });
        }
    }

    ui.checkbox(
        &mut state.cable.fixed_length,
        "Largo fijo Brother (bandera 90 mm, envolvente 39 mm)",
    );

    ui.horizontal(|ui| {
        ui.selectable_value(&mut state.cable.source, CableSource::List, "Lista");
        ui.selectable_value(&mut state.cable.source, CableSource::Sequence, "Serie");
        ui.selectable_value(&mut state.cable.source, CableSource::Table, "Tabla");
    });

    let table_labels = if state.cable.source == CableSource::Table {
        cable::parse_label_table(&state.cable.table, state.cable.table_header).ok()
    } else {
        None
    };
    if let Some(labels) = &table_labels
        && let Some(first) = labels.first()
        && let Ok(lines) = cable::split_cable_lines(first)
    {
        state.cable.line_count = lines.len() as u8;
    }

    if state.cable.source != CableSource::Table {
        ui.horizontal(|ui| {
            ui.label("Líneas");
            let mut count = state.cable.line_count;
            if ui
                .add(egui::DragValue::new(&mut count).range(1..=3))
                .changed()
            {
                state.cable.line_count = count;
                state.cable.id_line = count.saturating_sub(1);
            }
        });
    } else if let Some(labels) = &table_labels {
        ui.label(format!(
            "{} etiquetas, {} líneas",
            labels.len(),
            state.cable.line_count
        ));
    }
    if state.cable.id_line >= state.cable.line_count {
        state.cable.id_line = state.cable.line_count.saturating_sub(1);
    }
    ui.checkbox(&mut state.cable.same_size, "Mismo tamaño");
    if !state.cable.same_size {
        let tape_px = state.tape_width_px.max(1);
        for index in 0..state.cable.line_count as usize {
            ui.horizontal(|ui| {
                ui.label(format!("Línea {} alto", index + 1));
                ui.add(
                    egui::DragValue::new(&mut state.cable.line_heights[index])
                        .range(4..=tape_px)
                        .suffix(" px"),
                );
            });
        }
        let used: u32 = state.cable.line_heights[..state.cable.line_count as usize]
            .iter()
            .sum();
        ui.label(format!("{used} px de {tape_px} px de cinta"));
    }

    if state.cable.source == CableSource::Sequence {
        ui.horizontal(|ui| {
            ui.label("Prefijo");
            ui.text_edit_singleline(&mut state.cable.prefix);
        });
        ui.horizontal(|ui| {
            ui.label("Desde");
            ui.add(egui::DragValue::new(&mut state.cable.from).range(0..=999_999));
            ui.label("Cantidad");
            ui.add(egui::DragValue::new(&mut state.cable.count).range(1..=500));
            ui.label("Ceros");
            ui.add(egui::DragValue::new(&mut state.cable.digits).range(0..=8));
        });
        if state.cable.line_count > 1 {
            ui.label("El número va en");
            ui.horizontal(|ui| {
                for index in 0..state.cable.line_count {
                    ui.selectable_value(&mut state.cable.id_line, index, format!("{}", index + 1));
                }
            });
            for index in 0..state.cable.line_count as usize {
                if index == state.cable.id_line as usize {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.label(format!("Línea {}", index + 1));
                    ui.text_edit_singleline(&mut state.cable.fixed_lines[index]);
                });
            }
        }
    } else if state.cable.source == CableSource::Table {
        ui.checkbox(
            &mut state.cable.table_header,
            "La primera fila es encabezado",
        );
        ui.label("Cada fila es una etiqueta y cada columna una línea.");
        ui.add(
            egui::TextEdit::multiline(&mut state.cable.table)
                .desired_rows(6)
                .desired_width(f32::INFINITY)
                .hint_text("nombre0\taddr"),
        );
        if ui.button("Abrir Excel…").clicked() {
            open_table(state);
        }
    } else {
        ui.label("Una etiqueta por renglón. Separa sus líneas con |");
        ui.add(
            egui::TextEdit::multiline(&mut state.cable.list)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
    }

    ui.add_space(4.0);
    if ui.button("Generar etiquetas").clicked() {
        generate(state);
    }
}

fn generate(state: &mut AppState) {
    let line_count = state.cable.line_count.clamp(1, 3) as usize;
    let texts = match state.cable.source {
        CableSource::Sequence => cable::expand_ids(
            &state.cable.prefix,
            state.cable.from,
            state.cable.count,
            state.cable.digits,
        )
        .into_iter()
        .map(|id| {
            sequence_label(
                &id,
                line_count,
                state.cable.id_line,
                &state.cable.fixed_lines,
            )
        })
        .collect(),
        CableSource::List => cable::parse_label_list(&state.cable.list),
        CableSource::Table => {
            match cable::parse_label_table(&state.cable.table, state.cable.table_header) {
                Ok(labels) => {
                    if let Some(first) = labels.first()
                        && let Ok(lines) = cable::split_cable_lines(first)
                    {
                        state.cable.line_count = lines.len() as u8;
                    }
                    labels
                }
                Err(err) => {
                    state.status_message = err.to_string();
                    return;
                }
            }
        }
    };
    let line_count = state.cable.line_count.clamp(1, 3) as usize;
    if texts.is_empty() {
        state.status_message = "No hay etiquetas para generar".to_string();
        return;
    }
    for text in &texts {
        match cable::split_cable_lines(text) {
            Ok(lines) if lines.len() == line_count => {}
            Ok(lines) => {
                state.status_message = format!(
                    "\"{text}\" tiene {} línea(s) y el formulario pide {line_count}",
                    lines.len()
                );
                return;
            }
            Err(err) => {
                state.status_message = err.to_string();
                return;
            }
        }
    }
    let heights: Vec<u32> = state.cable.line_heights[..line_count].to_vec();
    let line_heights = if state.cable.same_size {
        None
    } else {
        Some(heights.as_slice())
    };

    let style = match state.cable.kind {
        CableKind::Flag => CableStyle::Flag {
            diameter_mm: state.cable.diameter_mm,
            slack_mm: state.cable.slack_mm,
            length_mm: state.cable.fixed_length.then_some(cable::BROTHER_FLAG_MM),
        },
        CableKind::Wrap => CableStyle::Wrap {
            diameter_mm: state.cable.diameter_mm,
            overlap_mm: state.cable.overlap_mm,
            length_mm: state.cable.fixed_length.then_some(cable::BROTHER_WRAP_MM),
        },
    };

    let mut renderer = TextRenderer::new();
    match cable::layout_rendered(
        &texts,
        style,
        state.printer_dpi,
        state.tape_width_px,
        &state.font_name,
        &mut renderer,
        line_heights,
    ) {
        Ok(elements) => {
            let count = texts.len();
            state.elements = elements;
            state.selected_element = None;
            state.mark_dirty();
            state.status_message = format!("Generadas {count} etiqueta(s) de cable");
            info!("{}", state.status_message);
        }
        Err(err) => {
            error!("Cable layout failed: {err}");
            state.status_message = err.to_string();
        }
    }
}

fn sequence_label(id: &str, line_count: usize, id_line: u8, fixed_lines: &[String; 3]) -> String {
    let id_at = (id_line as usize).min(line_count - 1);
    (0..line_count)
        .map(|index| {
            if index == id_at {
                id.to_string()
            } else {
                fixed_lines[index].trim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn open_table(state: &mut AppState) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Tabla", &["xlsx", "csv", "tsv"])
        .pick_file()
    else {
        return;
    };
    match load_table_text(&path) {
        Ok(text) => state.cable.table = text,
        Err(err) => {
            error!("No se pudo abrir la tabla: {err}");
            state.status_message = err;
        }
    }
}

fn load_table_text(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "xlsx" {
        let rows = xlsx_rows(path)?;
        return Ok(cable::table_as_text(&rows));
    }
    std::fs::read_to_string(path).map_err(|err| err.to_string())
}

fn xlsx_rows(path: &Path) -> Result<Vec<Vec<String>>, String> {
    use calamine::Reader;

    let mut workbook = calamine::open_workbook_auto(path).map_err(|err| err.to_string())?;
    let sheet = workbook
        .sheet_names()
        .into_iter()
        .next()
        .ok_or_else(|| "el archivo no tiene hojas".to_string())?;
    let range = workbook
        .worksheet_range(&sheet)
        .map_err(|err| err.to_string())?;
    Ok(range
        .rows()
        .map(|row| row.iter().map(cell_text).collect())
        .collect())
}

fn cell_text(cell: &calamine::Data) -> String {
    match cell {
        calamine::Data::Empty => String::new(),
        calamine::Data::String(text) => text.clone(),
        calamine::Data::Float(value) => cable::format_sheet_number(*value),
        calamine::Data::Int(value) => value.to_string(),
        calamine::Data::Bool(value) => value.to_string(),
        calamine::Data::DateTimeIso(text) | calamine::Data::DurationIso(text) => text.clone(),
        calamine::Data::DateTime(value) => value.to_string(),
        calamine::Data::Error(_) => String::new(),
    }
}
