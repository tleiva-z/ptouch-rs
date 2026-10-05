// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Command-line tool for Brother P-Touch label printers.
//!
//! Supports printing text labels, images, or combinations of both.
//! Can also export labels to image files (PNG, JPEG, BMP, etc.) for preview.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process;

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use log::debug;

#[cfg(target_os = "macos")]
use ptouch_core::BluetoothDevice;
use ptouch_core::PrinterStatus;
use ptouch_core::device::{self, DeviceFlags, DeviceInfo};
use ptouch_core::error::PtouchError;
use ptouch_core::protocol::PrintQuality;
use ptouch_core::tape;
use ptouch_core::transport::PtouchDevice;

use ptouch_render::bitmap::LabelBitmap;
use ptouch_render::cable::{self, CableStyle};
use ptouch_render::document::{self, LabelDocument};
use ptouch_render::image_loader;
use ptouch_render::raster;
use ptouch_render::text::{TextAlign, TextRenderer};

// ---------------------------------------------------------------------------
// CLI argument definitions
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "ptouch", version, about = "Brother P-Touch label printer tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
// Print carries many options; it is constructed once at startup, so the size
// difference between subcommands does not matter here.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Print labels with text, images, or both
    Print(PrintArgs),
    /// Show printer and tape information
    Info(InfoArgs),
    /// List supported printer models
    List,
    /// List devices paired in macOS Bluetooth settings
    BluetoothList,
    /// Launch GUI mode
    Gui,
    /// Etiquetas de cable: bandera o envolvente
    Cable(CableArgs),
}

#[derive(clap::Args)]
struct PrintArgs {
    /// Use an already-paired PT-P300BT at this Bluetooth address (macOS only)
    #[arg(long, value_name = "ADDRESS")]
    bluetooth: Option<String>,

    /// Text lines to print (each argument = one line, max 4)
    #[arg(value_name = "TEXT")]
    text: Vec<String>,

    /// Print a saved layout file (.ptl). The layout is authoritative; ad-hoc
    /// content flags (text, --image, --font, --size, --align, --margin, --cut,
    /// --pad) are ignored with a warning.
    #[arg(short = 'l', long, value_name = "FILE")]
    layout: Option<String>,

    /// Set a layout placeholder value (repeatable): --set name=Alice
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Print one label per row of a CSV file ('-' for stdin); the header row
    /// names the placeholders. With --output, include '{n}' for the row number.
    #[arg(long, value_name = "FILE")]
    csv: Option<String>,

    /// List the placeholders a layout declares, then exit
    #[arg(long)]
    list_vars: bool,

    /// Render placeholders with no value as blank instead of erroring
    #[arg(long)]
    allow_missing: bool,

    /// Print an image file
    #[arg(short = 'i', long)]
    image: Option<String>,

    /// Binarization mode for images
    #[arg(long, value_enum, default_value = "auto")]
    binarize: BinarizeArg,

    /// Export to PNG file instead of printing
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Font name
    #[arg(short = 'f', long, default_value = "DejaVuSans")]
    font: String,

    /// Font size in points (auto-detected if not set)
    #[arg(short = 's', long)]
    size: Option<f32>,

    /// Font top/bottom margin in pixels
    #[arg(short = 'm', long, default_value = "0")]
    margin: u32,

    /// Text alignment
    #[arg(short = 'a', long, value_enum, default_value = "left")]
    align: AlignArg,

    /// Force tape width in pixels (use with -o for image export without printer)
    #[arg(short = 'w', long)]
    tape_width: Option<u32>,

    /// Add a cut mark
    #[arg(short = 'c', long)]
    cut: bool,

    /// Add padding in pixels
    #[arg(short = 'p', long)]
    pad: Option<u32>,

    /// Mirror the whole label left-right (horizontal). With --layout, the
    /// layout's saved flip wins and this is ignored with a warning.
    #[arg(long)]
    flip_h: bool,

    /// Mirror the whole label top-bottom (vertical). With --layout, the
    /// layout's saved flip wins and this is ignored with a warning.
    #[arg(long)]
    flip_v: bool,

    /// Skip final feed and cut (for chained labels)
    #[arg(long)]
    chain: bool,

    /// Cut before label
    #[arg(long)]
    precut: bool,

    /// Print quality (high and draft need a printer with quality modes)
    #[arg(long, value_enum, default_value = "standard")]
    quality: QualityArg,

    /// Number of copies
    #[arg(long, default_value = "1")]
    copies: u32,

    /// Printer timeout in seconds
    #[arg(long, default_value = "1")]
    timeout: u32,

    /// Enable debug output
    #[arg(long)]
    debug: bool,
}

#[derive(clap::Args)]
struct CableArgs {
    #[command(subcommand)]
    action: CableAction,
}

#[derive(Subcommand)]
enum CableAction {
    /// Etiqueta bandera: texto, hueco del cable y el mismo texto girado 180°
    Flag(CableLabelArgs),
    /// Etiqueta envolvente: el largo alcanza para dar la vuelta al cable
    Wrap(CableLabelArgs),
}

#[derive(clap::Args)]
struct CableLabelArgs {
    /// Diámetro del cable en milímetros
    #[arg(long, default_value_t = 6.0)]
    diameter: f64,

    /// Holgura extra después del perímetro, en milímetros (bandera)
    #[arg(long, default_value_t = 2.0)]
    slack: f64,

    /// Solape extra, en milímetros (envolvente)
    #[arg(long, default_value_t = 10.0)]
    overlap: f64,

    /// Largo fijo Brother: 90 mm en bandera, 39 mm en envolvente
    #[arg(long)]
    fixed: bool,

    /// Un texto por etiqueta. `|` separa hasta 3 líneas. Se puede repetir.
    #[arg(long = "text")]
    text: Vec<String>,

    /// Alto en píxeles de cada línea, en orden. Sin esto, se reparten la cinta.
    #[arg(long = "height", value_name = "PX")]
    height: Vec<u32>,

    /// Texto fijo que se repite en la serie. Se usa junto con --prefix.
    #[arg(long = "line")]
    line: Vec<String>,

    /// Línea (1 a 3) donde va el número de la serie. 0, el valor por defecto, es la última.
    #[arg(long, default_value_t = 0)]
    id_line: u32,

    /// Prefijo de una serie numerada, por ejemplo CBL-
    #[arg(long)]
    prefix: Option<String>,

    /// Primer número de la serie
    #[arg(long, default_value_t = 1)]
    from: u32,

    /// Cantidad de etiquetas de la serie
    #[arg(long, default_value_t = 1)]
    count: u32,

    /// Ceros a la izquierda. 0 no rellena.
    #[arg(long, default_value_t = 3)]
    digits: u32,

    /// Tabla: cada fila es una etiqueta y cada columna una línea. CSV, TSV o .xlsx.
    #[arg(long, value_name = "FILE")]
    csv: Option<String>,

    /// La primera fila de --csv es encabezado y no se imprime.
    #[arg(long)]
    header: bool,

    /// Guardar un PNG en vez de imprimir
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Ancho de cinta en milímetros. Bandera: 12. Envolvente: 18 o 24.
    #[arg(long, default_value_t = 12)]
    tape_mm: u8,

    /// Fuente
    #[arg(short = 'f', long, default_value = "DejaVuSans")]
    font: String,

    /// Copias de la tira completa
    #[arg(long, default_value_t = 1)]
    copies: u32,

    /// No cortar al final de la tira
    #[arg(long)]
    chain: bool,

    /// Enable debug output
    #[arg(long)]
    debug: bool,
}

#[derive(clap::Args)]
struct InfoArgs {
    /// Use an already-paired PT-P300BT at this Bluetooth address (macOS only)
    #[arg(long, value_name = "ADDRESS")]
    bluetooth: Option<String>,

    /// Enable debug output
    #[arg(long)]
    debug: bool,

    /// Printer timeout in seconds
    #[arg(long, default_value = "1")]
    timeout: u32,
}

/// The printer selected by the CLI. USB remains the default target.
enum CliDevice {
    Usb(PtouchDevice),
    #[cfg(target_os = "macos")]
    Bluetooth(BluetoothDevice),
}

impl CliDevice {
    fn open(bluetooth: Option<&str>) -> Result<Self, PtouchError> {
        if let Some(address) = bluetooth {
            #[cfg(target_os = "macos")]
            {
                return BluetoothDevice::open(address).map(Self::Bluetooth);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = address;
                return Err(PtouchError::UnsupportedOperation(
                    "--bluetooth is available on macOS only",
                ));
            }
        }
        PtouchDevice::open_first().map(Self::Usb)
    }

    fn init(&mut self) -> Result<(), PtouchError> {
        match self {
            Self::Usb(device) => device.init(),
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.init(),
        }
    }

    fn status(&self) -> Option<&PrinterStatus> {
        match self {
            Self::Usb(device) => device.status(),
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.status(),
        }
    }

    fn model_name(&self) -> &'static str {
        match self {
            Self::Usb(device) => device.device_info().name,
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.model_name(),
        }
    }

    fn dpi(&self) -> u16 {
        match self {
            Self::Usb(device) => device.device_info().dpi,
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.dpi(),
        }
    }

    fn tape_width_px(&self) -> Option<u16> {
        match self {
            Self::Usb(device) => device.tape_width_px(),
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.tape_width_px(),
        }
    }

    fn raster_width_px(&self) -> u16 {
        match self {
            Self::Usb(device) => device.max_px(),
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.raster_width_px(),
        }
    }

    fn is_bluetooth(&self) -> bool {
        match self {
            Self::Usb(_) => false,
            #[cfg(target_os = "macos")]
            Self::Bluetooth(_) => true,
        }
    }

    fn print_raster(
        &mut self,
        lines: &[Vec<u8>],
        chain_print: bool,
        precut: bool,
        quality: PrintQuality,
    ) -> Result<(), PtouchError> {
        self.print_pages(&[lines], chain_print, precut, quality, false)
    }

    fn print_pages(
        &mut self,
        pages: &[&[Vec<u8>]],
        chain_print: bool,
        precut: bool,
        quality: PrintQuality,
        cut_between: bool,
    ) -> Result<(), PtouchError> {
        match self {
            Self::Usb(device) => {
                device.print_pages(pages, chain_print, precut, quality, cut_between)
            }
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => {
                for page in pages {
                    device.print_raster(page)?;
                }
                Ok(())
            }
        }
    }

    fn close(self) -> Result<(), PtouchError> {
        match self {
            Self::Usb(device) => device.close(),
            #[cfg(target_os = "macos")]
            Self::Bluetooth(device) => device.close(),
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum AlignArg {
    Left,
    Center,
    Right,
}

impl AlignArg {
    fn to_text_align(self) -> TextAlign {
        match self {
            AlignArg::Left => TextAlign::Left,
            AlignArg::Center => TextAlign::Center,
            AlignArg::Right => TextAlign::Right,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum QualityArg {
    Standard,
    High,
    Draft,
}

impl QualityArg {
    fn to_print_quality(self) -> PrintQuality {
        match self {
            QualityArg::Standard => PrintQuality::Standard,
            QualityArg::High => PrintQuality::HighRes,
            QualityArg::Draft => PrintQuality::Draft,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum BinarizeArg {
    Auto,
    Threshold,
    Dither,
}

impl BinarizeArg {
    fn to_binarize_mode(self) -> image_loader::BinarizeMode {
        match self {
            BinarizeArg::Auto => image_loader::BinarizeMode::Auto,
            BinarizeArg::Threshold => image_loader::BinarizeMode::Threshold,
            BinarizeArg::Dither => image_loader::BinarizeMode::Dither,
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

fn main() {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    match cli.command {
        Commands::List => execute_list(),
        Commands::BluetoothList => {
            if let Err(e) = execute_bluetooth_list() {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
        Commands::Gui => execute_gui(),
        Commands::Info(args) => {
            init_logging(args.debug);
            if let Err(e) = execute_info(&args) {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
        Commands::Cable(args) => {
            let debug = match &args.action {
                CableAction::Flag(inner) | CableAction::Wrap(inner) => inner.debug,
            };
            init_logging(debug);
            if let Err(e) = execute_cable(args) {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
        Commands::Print(args) => {
            init_logging(args.debug);
            // When a layout drives the label, ad-hoc content flags do not
            // apply; collect the ones the user typed so we can warn.
            let ignored = if args.layout.is_some() {
                ignored_content_flags(&matches)
            } else {
                Vec::new()
            };
            if let Err(e) = execute_print(&args, &ignored) {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
    }
}

/// Ad-hoc content flags overridden when `--layout` is set. Keep in sync with
/// `PrintArgs`; the `content_flag_ids_resolve` test guards against renames.
const CONTENT_FLAG_IDS: &[&str] = &[
    "text", "image", "font", "size", "align", "margin", "cut", "pad", "flip_h", "flip_v",
];

/// Return the display names of content flags the user explicitly passed on the
/// command line (defaults do not count), for the warn-and-ignore message.
fn ignored_content_flags(matches: &ArgMatches) -> Vec<String> {
    let Some(sub) = matches.subcommand_matches("print") else {
        return Vec::new();
    };
    CONTENT_FLAG_IDS
        .iter()
        .filter(|id| sub.value_source(id) == Some(ValueSource::CommandLine))
        .map(|id| {
            if *id == "text" {
                "TEXT".to_string()
            } else {
                // clap derives long flags with hyphens (flip_h -> --flip-h).
                format!("--{}", id.replace('_', "-"))
            }
        })
        .collect()
}

/// Parse `--set KEY=VALUE` arguments into a map. Errors on an entry with no `=`.
fn parse_set_args(set: &[String]) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let mut values = BTreeMap::new();
    for entry in set {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid --set '{}' (expected KEY=VALUE)", entry))?;
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

/// Check provided values against the placeholders a layout declares.
///
/// Warns once about values that the layout does not use, and (unless
/// `allow_missing`) errors when a declared placeholder has no value.
fn validate_vars(
    declared: &[String],
    provided: &BTreeSet<String>,
    allow_missing: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let declared_set: BTreeSet<&String> = declared.iter().collect();

    let unused: Vec<&String> = provided
        .iter()
        .filter(|name| !declared_set.contains(name))
        .collect();
    if !unused.is_empty() {
        let names: Vec<&str> = unused.iter().map(|s| s.as_str()).collect();
        eprintln!(
            "WARN: value(s) not used by the layout: {}",
            names.join(", ")
        );
    }

    if !allow_missing {
        let missing: Vec<&str> = declared
            .iter()
            .filter(|name| !provided.contains(*name))
            .map(|s| s.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing value(s) for placeholder(s): {} (pass --set or --allow-missing)",
                missing.join(", ")
            )
            .into());
        }
    }
    Ok(())
}

/// Initialize env_logger with optional debug level.
fn init_logging(debug: bool) {
    let level = if debug { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp(None)
        .init();
}

// ---------------------------------------------------------------------------
// Subcommand: list
// ---------------------------------------------------------------------------

/// Print a table of all supported printer models.
fn execute_list() {
    let devices = device::supported_devices();
    println!(
        "Supported Brother P-Touch printers ({} models):",
        devices.len()
    );
    println!();
    println!(
        "  {:<30} {:>6} {:>6} {:>4}  Max Pixels",
        "Model", "VID", "PID", "DPI"
    );
    println!("  {}", "-".repeat(70));
    for dev in devices {
        let flags = format_flags(dev);
        println!(
            "  {:<30} 0x{:04x} 0x{:04x} {:>4}  {:>6}  {}",
            dev.name, dev.vid, dev.pid, dev.dpi, dev.max_px, flags
        );
    }
}

/// List paired devices and the stable addresses accepted by `--bluetooth`.
fn execute_bluetooth_list() -> Result<(), PtouchError> {
    #[cfg(target_os = "macos")]
    {
        let devices = BluetoothDevice::paired_devices()?;
        if devices.is_empty() {
            println!("No paired Bluetooth devices found.");
        } else {
            println!("Paired Bluetooth devices:");
            for device in devices {
                println!("  {}  {}", device.address, device.name);
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(PtouchError::UnsupportedOperation(
            "bluetooth-list is available on macOS only",
        ))
    }
}

/// Format device flags into a human-readable string.
fn format_flags(dev: &DeviceInfo) -> String {
    let mut parts = Vec::new();
    if dev.flags.contains(DeviceFlags::RASTER_PACKBITS) {
        parts.push("packbits");
    }
    if dev.flags.contains(DeviceFlags::HAS_PRECUT) {
        parts.push("precut");
    }
    if dev.flags.contains(DeviceFlags::P700_INIT) {
        parts.push("p700-init");
    }
    if dev.flags.contains(DeviceFlags::USE_INFO_CMD) {
        parts.push("info-cmd");
    }
    if dev.flags.contains(DeviceFlags::PLITE) {
        parts.push("plite");
    }
    if dev.flags.contains(DeviceFlags::UNSUP_RASTER) {
        parts.push("no-raster");
    }
    if dev.flags.contains(DeviceFlags::D460BT_MAGIC) {
        parts.push("d460bt");
    }
    if dev.flags.contains(DeviceFlags::WAIT_FOR_RECEIVE_READY) {
        parts.push("wait-ready");
    }
    if dev.flags.contains(DeviceFlags::AUTO_STATUS_NOTIFICATION) {
        parts.push("auto-status");
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]", parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Subcommand: gui
// ---------------------------------------------------------------------------

/// Print a message directing users to the GUI application.
fn execute_gui() {
    println!("Use ptouch-gui for the graphical interface.");
}

// ---------------------------------------------------------------------------
// Subcommand: info
// ---------------------------------------------------------------------------

/// Open the printer and display status and tape information.
fn execute_info(args: &InfoArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut dev = CliDevice::open(args.bluetooth.as_deref())?;
    dev.init()?;

    // init() already called get_status() internally; use that result.
    let status = dev
        .status()
        .ok_or_else(|| PtouchError::StatusError("No status available after init".to_string()))?
        .clone();

    println!("Printer Information");
    println!("  Model:          {}", dev.model_name());
    println!("  Status:         {}", status.status_type_name());
    println!("  Media type:     {}", status.media_type_name());
    println!("  Media width:    {} mm", status.media_width);
    println!("  Tape color:     {}", status.tape_color_name());
    println!("  Text color:     {}", status.text_color_name());

    if status.has_error() {
        println!("  Errors:         {}", status.error_description());
    }

    let tape_width_px = dev.tape_width_px();
    let max_px = dev.raster_width_px();
    let dpi = dev.dpi();

    println!();
    println!("Tape Details");
    if let Some(px) = tape_width_px {
        println!("  Tape width:     {} px", px);
    } else {
        println!("  Tape width:     unknown");
    }
    if dev.is_bluetooth() {
        println!("  Raster width:   {} px", max_px);
    } else {
        println!("  Max printable:  {} px", max_px);
    }
    println!("  Resolution:     {} DPI", dpi);

    // Look up tape info by the reported media width. The pixel value from
    // the transport is clamped to the head width, so it cannot be used as
    // a reverse lookup key.
    if !dev.is_bluetooth()
        && let Some(t) = tape::find_tape(status.media_width, dpi)
    {
        println!("  Tape size:      {} mm", t.width_mm);
        println!("  Margin:         {:.1} mm", t.margin_mm);
    }

    dev.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: print
// ---------------------------------------------------------------------------

/// Build a label from a layout file or ad-hoc text/images, then print or save.
fn execute_print(args: &PrintArgs, ignored: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.tape_width == Some(0) {
        eprintln!("Error: --tape-width must be greater than 0");
        process::exit(1);
    }

    // Layout-only modifiers make no sense without a layout.
    if args.layout.is_none()
        && (!args.set.is_empty() || args.csv.is_some() || args.list_vars || args.allow_missing)
    {
        eprintln!("Error: --set, --csv, --list-vars, and --allow-missing require --layout");
        process::exit(1);
    }

    validate_bluetooth_print_options(args)?;

    if let Some(layout_path) = args.layout.as_deref() {
        if !ignored.is_empty() {
            eprintln!("WARN: --layout is set; ignoring: {}", ignored.join(", "));
        }
        return print_layout(args, layout_path);
    }

    // Validate arguments
    if args.text.is_empty() && args.image.is_none() {
        eprintln!("Error: nothing to print (provide text, --image, or --layout)");
        process::exit(1);
    }

    if args.text.len() > 4 {
        eprintln!("Error: at most 4 text lines are supported");
        process::exit(1);
    }

    if args.tape_width.is_some() && args.output.is_none() {
        eprintln!("Error: --tape-width requires --output");
        process::exit(1);
    }

    // Determine the print width and optionally open the device
    let (print_width, max_px, mut device): (u32, u16, Option<CliDevice>) =
        if let Some(w) = args.tape_width {
            // PNG-only mode, no printer needed
            debug!("PNG-only mode with forced tape width: {} px", w);
            (w, w as u16, None)
        } else {
            // Connect to the printer
            debug!("Connecting to printer...");
            let mut dev = CliDevice::open(args.bluetooth.as_deref())?;
            dev.init()?;
            // init() already called get_status() internally
            let width = dev.tape_width_px().ok_or_else(|| {
                PtouchError::StatusError("Could not determine tape width".to_string())
            })?;
            let max = dev.raster_width_px();
            debug!("Printer tape width: {} px, max: {} px", width, max);
            (u32::from(width), max, Some(dev))
        };

    // Whole-label mirroring applies once, after the label is composed.
    let bitmap = build_label(args, print_width)?.mirrored(args.flip_h, args.flip_v);
    emit_label(&bitmap, args, max_px, device.as_mut())?;

    if let Some(dev) = device {
        dev.close()?;
    }

    Ok(())
}

/// Reject combinations that the physically verified PT-P300BT path does not
/// implement. Do this before opening the printer or rendering the label.
fn validate_bluetooth_print_options(args: &PrintArgs) -> Result<(), PtouchError> {
    if args.bluetooth.is_none() {
        return Ok(());
    }
    if args.chain {
        return Err(PtouchError::UnsupportedOperation(
            "--chain is not supported by PT-P300BT",
        ));
    }
    if args.precut {
        return Err(PtouchError::UnsupportedOperation(
            "--precut is not supported by PT-P300BT, which has a manual cutter",
        ));
    }
    if args.quality != QualityArg::Standard {
        return Err(PtouchError::UnsupportedOperation(
            "PT-P300BT supports standard print quality only",
        ));
    }
    Ok(())
}

/// Load a `.ptl` layout, render it, and either print it or save as an image.
fn print_layout(args: &PrintArgs, layout_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(layout_path)?;
    let mut doc = LabelDocument::from_toml_str(&text)?;

    if args.list_vars {
        for name in doc.placeholders() {
            println!("{}", name);
        }
        return Ok(());
    }

    if let Some(csv_path) = args.csv.as_deref() {
        return print_layout_batch(args, doc, csv_path);
    }

    // Fill placeholders from --set values, rejecting missing ones by default.
    let values = parse_set_args(&args.set)?;
    let provided: BTreeSet<String> = values.keys().cloned().collect();
    validate_vars(&doc.placeholders(), &provided, args.allow_missing)?;
    doc.apply_values(&values);

    let (print_width, max_px, mut device) = resolve_layout_target(args, &doc)?;
    let bitmap = render_layout(&doc, print_width)?;
    emit_label(&bitmap, args, max_px, device.as_mut())?;

    if let Some(dev) = device {
        dev.close()?;
    }

    Ok(())
}

/// Print one label per CSV row, substituting the header columns (plus any
/// `--set` constants) into the layout placeholders.
fn print_layout_batch(
    args: &PrintArgs,
    doc: LabelDocument,
    csv_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(output) = &args.output
        && !output.contains("{n}")
    {
        eprintln!("Error: with --csv, --output must contain '{{n}}' (e.g. label-{{n}}.png)");
        process::exit(1);
    }

    let base = parse_set_args(&args.set)?;
    let reader: Box<dyn Read> = if csv_path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(csv_path)?)
    };
    let mut rdr = csv::Reader::from_reader(reader);
    let headers: Vec<String> = rdr.headers()?.iter().map(|s| s.to_string()).collect();

    // Validate the placeholders against the CSV columns plus any --set keys.
    let mut provided: BTreeSet<String> = headers.iter().cloned().collect();
    provided.extend(base.keys().cloned());
    validate_vars(&doc.placeholders(), &provided, args.allow_missing)?;

    let (print_width, max_px, mut device) = resolve_layout_target(args, &doc)?;

    let mut count = 0usize;
    for record in rdr.records() {
        let record = record?;
        let row: Vec<String> = record.iter().map(|s| s.to_string()).collect();
        let values = build_row_values(&base, &headers, &row);

        let mut row_doc = doc.clone();
        row_doc.apply_values(&values);
        let bitmap = render_layout(&row_doc, print_width)?;

        count += 1;
        if let Some(output) = &args.output {
            let path = output.replace("{n}", &count.to_string());
            bitmap.save(Path::new(&path))?;
            println!("Saved row {} to '{}'", count, path);
        } else if let Some(dev) = device.as_mut() {
            print_to_device(dev, &bitmap, max_px, args)?;
        } else {
            eprintln!("Error: no output destination (use --output or connect a printer)");
            process::exit(1);
        }
    }

    if let Some(dev) = device {
        dev.close()?;
    }

    if count == 0 {
        eprintln!("WARN: CSV had no data rows; nothing printed");
    } else {
        println!("Processed {} row(s)", count);
    }
    Ok(())
}

/// Merge `--set` constants with one CSV row's columns (row values win).
fn build_row_values(
    base: &BTreeMap<String, String>,
    headers: &[String],
    record: &[String],
) -> BTreeMap<String, String> {
    let mut values = base.clone();
    for (header, value) in headers.iter().zip(record.iter()) {
        values.insert(header.clone(), value.clone());
    }
    values
}

/// Render a (placeholder-resolved) layout document to a single bitmap.
fn render_layout(
    doc: &LabelDocument,
    print_width: u32,
) -> Result<LabelBitmap, Box<dyn std::error::Error>> {
    let mut renderer = TextRenderer::new();
    let bitmap = document::render_elements(
        &doc.elements,
        print_width,
        &doc.font_name,
        doc.font_margin,
        &mut renderer,
    )?
    .ok_or_else(|| PtouchError::SendFailed("layout produced no output".to_string()))?;
    // The layout's saved whole-label flip is applied after composition.
    Ok(bitmap.mirrored(doc.flip_h, doc.flip_v))
}

/// Resolve the print width, max pixels, and optional device for a layout.
///
///   - `--tape-width`: forced PNG width (requires `--output`)
///   - `--output` only: PNG export at the saved width, no printer needed
///   - otherwise: print at the printer's actual width, warn on mismatch
fn resolve_layout_target(
    args: &PrintArgs,
    doc: &LabelDocument,
) -> Result<(u32, u16, Option<CliDevice>), Box<dyn std::error::Error>> {
    // Offline export renders at the resolution the layout was designed at;
    // printing uses the printer's own status-derived width.
    let saved_px = tape::find_tape(doc.tape_width_mm, doc.dpi).map(|t| u32::from(t.pixels));

    if let Some(w) = args.tape_width {
        if args.output.is_none() {
            eprintln!("Error: --tape-width requires --output");
            process::exit(1);
        }
        Ok((w, w as u16, None))
    } else if args.output.is_some() {
        let w = saved_px.ok_or_else(|| {
            PtouchError::StatusError("unknown saved tape width; pass --tape-width".to_string())
        })?;
        Ok((w, w as u16, None))
    } else {
        let mut dev = CliDevice::open(args.bluetooth.as_deref())?;
        dev.init()?;
        let printer_px = u32::from(dev.tape_width_px().ok_or_else(|| {
            PtouchError::StatusError("Could not determine tape width".to_string())
        })?);
        // Warn on a real tape width mismatch. A pixel difference alone just
        // means the printer resolution differs from the design resolution,
        // and the refit to printer_px already handles that.
        let printer_mm = dev.status().map(|s| s.media_width).unwrap_or(0);
        if printer_mm > 0 && printer_mm != doc.tape_width_mm {
            eprintln!(
                "WARN: layout saved for {}mm, printer has {}mm; refitting to printer tape",
                doc.tape_width_mm, printer_mm
            );
        } else if printer_mm == 0 && saved_px.is_some_and(|s| s != printer_px) {
            eprintln!(
                "WARN: layout saved for {}mm tape; refitting to the printer tape",
                doc.tape_width_mm
            );
        }
        let max = dev.raster_width_px();
        Ok((printer_px, max, Some(dev)))
    }
}

/// Save a rendered label to an image file or print it to the device.
fn emit_label(
    bitmap: &LabelBitmap,
    args: &PrintArgs,
    max_px: u16,
    device: Option<&mut CliDevice>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(ref output_path) = args.output {
        bitmap.save(Path::new(output_path))?;
        let dpi = device.as_ref().map_or(180, |d| d.dpi());
        let tape_mm = bitmap.width() as f64 / f64::from(dpi) * 25.4;
        println!(
            "Saved to '{}' ({}x{} px, {:.1} mm of tape)",
            output_path,
            bitmap.width(),
            bitmap.height(),
            tape_mm
        );
    } else if let Some(dev) = device {
        print_to_device(dev, bitmap, max_px, args)?;
    } else {
        eprintln!("Error: no output destination (use --output or connect a printer)");
        process::exit(1);
    }
    Ok(())
}

/// Compose a label bitmap from text, image, cut marks, and padding.
fn build_label(
    args: &PrintArgs,
    print_width: u32,
) -> Result<LabelBitmap, Box<dyn std::error::Error>> {
    let mut result: Option<LabelBitmap> = None;

    // Render text if provided
    if !args.text.is_empty() {
        let mut renderer = TextRenderer::new();
        let lines: Vec<&str> = args.text.iter().map(|s| s.as_str()).collect();
        let align = args.align.to_text_align();

        debug!(
            "Rendering {} text line(s), font={}, size={:?}, margin={}, align={:?}",
            lines.len(),
            args.font,
            args.size,
            args.margin,
            args.align
        );

        let text_bitmap = renderer.render_text(
            &lines,
            print_width,
            &args.font,
            args.size,
            args.margin,
            align,
        )?;

        result = Some(append_bitmap(result, text_bitmap));
    }

    // Load and append image if provided
    if let Some(ref img_path) = args.image {
        debug!("Loading image: {}", img_path);
        let options = image_loader::ImageLoadOptions {
            binarize: args.binarize.to_binarize_mode(),
            target_height: Some(print_width),
            ..image_loader::ImageLoadOptions::default()
        };
        let img_bitmap = image_loader::load_image(Path::new(img_path), &options)?;
        result = Some(append_bitmap(result, img_bitmap));
    }

    // Add cut mark if requested
    if args.cut {
        debug!("Adding cut mark");
        let mark = make_cutmark(print_width);
        result = Some(append_bitmap(result, mark));
    }

    // Add padding if requested
    if let Some(pad_px) = args.pad {
        debug!("Adding {} px padding", pad_px);
        let pad = make_padding(print_width, pad_px);
        result = Some(append_bitmap(result, pad));
    }

    result.ok_or_else(|| {
        Box::new(PtouchError::SendFailed("No content to render".to_string()))
            as Box<dyn std::error::Error>
    })
}

/// Append a new bitmap to an existing one, or return the new bitmap if there
/// is no existing bitmap yet.
fn append_bitmap(existing: Option<LabelBitmap>, new: LabelBitmap) -> LabelBitmap {
    match existing {
        Some(prev) => prev.append(&new),
        None => new,
    }
}

/// Create a cut mark bitmap: a dashed vertical line.
///
/// The mark is 1 pixel wide with alternating black/white dots across the
/// tape height.
fn make_cutmark(print_width: u32) -> LabelBitmap {
    let mut bmp = LabelBitmap::new(1, print_width);
    for y in 0..print_width {
        // Alternating 2-pixel dashes
        if (y / 2) % 2 == 0 {
            bmp.set_pixel(0, y, true);
        }
    }
    bmp
}

/// Create a blank padding bitmap of the given width (in the print direction).
fn make_padding(print_width: u32, pad_px: u32) -> LabelBitmap {
    LabelBitmap::new(pad_px, print_width)
}

/// Send the label bitmap to the printer.
fn print_to_device(
    dev: &mut CliDevice,
    bitmap: &LabelBitmap,
    max_px: u16,
    args: &PrintArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let raster_lines = raster::bitmap_to_raster_lines(bitmap, max_px);

    let total_copies = args.copies.max(1);
    for copy_idx in 0..total_copies {
        let is_last = copy_idx == total_copies - 1;
        // Chain intermediate copies (no cut between copies).
        // Last copy: chain only if user requested --chain.
        // Chain intermediate copies; last copy follows user's --chain flag
        let chain_print = !dev.is_bluetooth() && (args.chain || !is_last);

        debug!(
            "Printing copy {}/{} ({} raster lines, chain={})",
            copy_idx + 1,
            total_copies,
            raster_lines.len(),
            chain_print
        );

        dev.print_raster(
            &raster_lines,
            chain_print,
            args.precut,
            args.quality.to_print_quality(),
        )?;
    }

    let tape_mm = bitmap.width() as f64 / f64::from(dev.dpi()) * 25.4;
    println!(
        "Printed {} cop{} ({:.1} mm of tape each)",
        total_copies,
        if total_copies == 1 { "y" } else { "ies" },
        tape_mm
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: cable
// ---------------------------------------------------------------------------

/// Build a flag or wrap strip and save it, or print it as one job.
fn execute_cable(args: CableArgs) -> Result<(), Box<dyn std::error::Error>> {
    let label = match &args.action {
        CableAction::Flag(inner) | CableAction::Wrap(inner) => inner,
    };
    let texts = resolve_cable_texts(label)?;
    let style = cable_style(&args.action, label);

    let (tape_px, dpi, max_px, device) = if label.output.is_some() {
        let px = tape::tape_pixels(label.tape_mm, cable::DESIGN_DPI)
            .ok_or_else(|| format!("ancho de cinta desconocido: {} mm", label.tape_mm))?;
        (u32::from(px), cable::DESIGN_DPI, px, None)
    } else {
        let mut dev = CliDevice::open(None)?;
        dev.init()?;
        let px = dev
            .tape_width_px()
            .ok_or("no se pudo leer el ancho de la cinta")?;
        let dpi = dev.dpi();
        let max = dev.raster_width_px();
        let printer_mm = dev.status().map(|status| status.media_width).unwrap_or(0);
        if printer_mm > 0 && printer_mm != label.tape_mm {
            eprintln!(
                "Aviso: pediste cinta de {} mm y la impresora tiene {} mm; se usa la cinta cargada",
                label.tape_mm, printer_mm
            );
        }
        (u32::from(px), dpi, max, Some(dev))
    };

    let mut renderer = TextRenderer::new();
    let line_heights = if label.height.is_empty() {
        None
    } else {
        Some(label.height.as_slice())
    };
    let elements = cable::layout_rendered(
        &texts,
        style,
        dpi,
        tape_px,
        &label.font,
        &mut renderer,
        line_heights,
    )?;
    let bitmap = document::render_elements(&elements, tape_px, &label.font, 0, &mut renderer)?
        .ok_or("la etiqueta de cable salió vacía")?;

    let length_mm = bitmap.width() as f64 / f64::from(dpi) * 25.4;
    // compose::cutmark is 9 px wide and sits between labels, not inside them.
    let cut_px = 9 * (texts.len().saturating_sub(1) as u32);
    let per_px = bitmap.width().saturating_sub(cut_px) / (texts.len().max(1) as u32);
    let per_mm = per_px as f64 / f64::from(dpi) * 25.4;
    if per_mm > 73.0 {
        eprintln!(
            "Aviso: cada etiqueta mide cerca de {per_mm:.0} mm (la tira completa, {length_mm:.0} mm). \
             En la PT-D600 hubo cortes cerca de los 73 mm; si una etiqueta se corta antes, \
             acorta el texto o no uses el largo fijo de 90 mm."
        );
    }

    if let Some(path) = &label.output {
        bitmap.save(Path::new(path))?;
        println!(
            "Guardado en '{path}' ({}x{} px, {length_mm:.1} mm de cinta, {} etiqueta(s))",
            bitmap.width(),
            bitmap.height(),
            texts.len()
        );
        return Ok(());
    }

    let Some(mut dev) = device else {
        return Err("indica --output o conecta la impresora".into());
    };
    let page_bitmaps = cable_page_bitmaps(&elements, tape_px, &label.font, &mut renderer)?;
    let rasters: Vec<Vec<Vec<u8>>> = page_bitmaps
        .iter()
        .map(|page| raster::bitmap_to_raster_lines(page, max_px))
        .collect();
    let page_refs: Vec<&[Vec<u8>]> = rasters.iter().map(Vec::as_slice).collect();
    let copies = label.copies.max(1);
    for copy in 0..copies {
        let is_last = copy + 1 == copies;
        // Intermediate copies stay chained. The last copy cuts unless asked not to.
        let chain = label.chain || !is_last;
        dev.print_pages(&page_refs, chain, false, PrintQuality::Standard, false)?;
    }
    println!(
        "Impresas {copies} copias ({length_mm:.1} mm de cinta cada una, {} etiqueta(s))",
        texts.len()
    );
    dev.close()?;
    Ok(())
}

fn cable_page_bitmaps(
    elements: &[ptouch_render::document::LabelElement],
    tape_px: u32,
    font: &str,
    renderer: &mut TextRenderer,
) -> Result<Vec<ptouch_render::bitmap::LabelBitmap>, Box<dyn std::error::Error>> {
    let mut pages = Vec::new();
    for slice in document::split_at_cut_marks(elements) {
        if let Some(bitmap) = document::render_elements(slice, tape_px, font, 0, renderer)? {
            pages.push(bitmap);
        }
    }
    if pages.is_empty() {
        return Err("la etiqueta de cable salió vacía".into());
    }
    Ok(pages)
}

fn cable_style(action: &CableAction, label: &CableLabelArgs) -> CableStyle {
    match action {
        CableAction::Flag(_) => CableStyle::Flag {
            diameter_mm: label.diameter,
            slack_mm: label.slack,
            length_mm: label.fixed.then_some(cable::BROTHER_FLAG_MM),
        },
        CableAction::Wrap(_) => CableStyle::Wrap {
            diameter_mm: label.diameter,
            overlap_mm: label.overlap,
            length_mm: label.fixed.then_some(cable::BROTHER_WRAP_MM),
        },
    }
}

fn resolve_cable_texts(args: &CableLabelArgs) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let sources = [
        args.csv.is_some(),
        args.prefix.is_some(),
        !args.text.is_empty(),
    ]
    .iter()
    .filter(|on| **on)
    .count();
    if sources == 0 {
        return Err("indica etiquetas con --text, --prefix o --csv".into());
    }
    if sources > 1 {
        return Err("usa solo una fuente: --text, --prefix o --csv".into());
    }
    if args.prefix.is_none() && (!args.line.is_empty() || args.id_line != 0) {
        return Err("--line y --id-line se usan con --prefix".into());
    }
    if args.header && args.csv.is_none() {
        return Err("--header se usa con --csv".into());
    }
    if let Some(path) = &args.csv {
        let labels = read_cable_table(path, args.header)?;
        if labels.is_empty() {
            return Err(format!("'{path}' no tiene etiquetas").into());
        }
        return Ok(labels);
    }
    if let Some(prefix) = &args.prefix {
        if args.count == 0 {
            return Err("--count debe ser mayor que 0".into());
        }
        return series_labels(args, prefix);
    }
    Ok(args.text.clone())
}

fn series_labels(
    args: &CableLabelArgs,
    prefix: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let ids = cable::expand_ids(prefix, args.from, args.count, args.digits);
    if args.line.is_empty() {
        if args.height.len() > 1 {
            return Err(
                "la serie tiene una línea; pasa --line para las otras o un solo --height".into(),
            );
        }
        return Ok(ids);
    }
    let line_count = args.line.len() + 1;
    if line_count > cable::MAX_CABLE_LINES {
        return Err(format!(
            "una etiqueta admite como máximo {} líneas",
            cable::MAX_CABLE_LINES
        )
        .into());
    }
    let id_at = if args.id_line == 0 {
        line_count - 1
    } else {
        args.id_line as usize - 1
    };
    if id_at >= line_count {
        return Err(format!("--id-line debe estar entre 1 y {line_count}").into());
    }
    Ok(ids
        .into_iter()
        .map(|id| {
            let mut fixed = args.line.iter();
            (0..line_count)
                .map(|index| {
                    if index == id_at {
                        id.clone()
                    } else {
                        fixed.next().map(String::as_str).unwrap_or("").to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect())
}

fn read_cable_table(path: &str, header: bool) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "xlsx" {
        let rows = xlsx_rows(path)?;
        return Ok(cable::labels_from_table(&rows, header)?);
    }
    let mut file = File::open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(cable::parse_label_table(&text, header)?)
}

fn xlsx_rows(path: &str) -> Result<Vec<Vec<String>>, Box<dyn std::error::Error>> {
    use calamine::Reader;

    let mut workbook = calamine::open_workbook_auto(path)?;
    let sheet = workbook
        .sheet_names()
        .into_iter()
        .next()
        .ok_or("el archivo no tiene hojas")?;
    let range = workbook.worksheet_range(&sheet)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn print_matches(argv: &[&str]) -> ArgMatches {
        Cli::command().get_matches_from(argv)
    }

    #[test]
    fn bluetooth_target_is_accepted_by_info_and_print() {
        let info =
            Cli::try_parse_from(["ptouch", "info", "--bluetooth", "AA:BB:CC:DD:EE:FF"]).unwrap();
        let Commands::Info(info) = info.command else {
            panic!("expected info command");
        };
        assert_eq!(info.bluetooth.as_deref(), Some("AA:BB:CC:DD:EE:FF"));

        let print = Cli::try_parse_from([
            "ptouch",
            "print",
            "--bluetooth",
            "AA:BB:CC:DD:EE:FF",
            "Hello",
        ])
        .unwrap();
        let Commands::Print(print) = print.command else {
            panic!("expected print command");
        };
        assert_eq!(print.bluetooth.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(print.text, ["Hello"]);
        assert!(validate_bluetooth_print_options(&print).is_ok());
    }

    #[test]
    fn bluetooth_list_command_is_accepted() {
        let cli = Cli::try_parse_from(["ptouch", "bluetooth-list"]).unwrap();
        assert!(matches!(cli.command, Commands::BluetoothList));
    }

    #[test]
    fn bluetooth_rejects_unimplemented_print_options_before_connecting() {
        for option in ["--chain", "--precut", "--quality=high"] {
            let cli = Cli::try_parse_from([
                "ptouch",
                "print",
                "--bluetooth",
                "AA:BB:CC:DD:EE:FF",
                option,
                "Hello",
            ])
            .unwrap();
            let Commands::Print(print) = cli.command else {
                panic!("expected print command");
            };
            assert!(
                matches!(
                    validate_bluetooth_print_options(&print),
                    Err(PtouchError::UnsupportedOperation(_))
                ),
                "option was unexpectedly accepted: {option}"
            );
        }
    }

    #[test]
    fn test_content_flag_ids_resolve() {
        let cmd = Cli::command();
        let print = cmd.find_subcommand("print").expect("print subcommand");
        for id in CONTENT_FLAG_IDS {
            assert!(
                print.get_arguments().any(|a| a.get_id() == *id),
                "content flag id '{}' not found in print args",
                id
            );
        }
    }

    #[test]
    fn test_typed_content_flags_are_reported() {
        let matches = print_matches(&[
            "ptouch", "print", "Hello", "-s", "24", "--flip-h", "-l", "x.ptl",
        ]);
        let ignored = ignored_content_flags(&matches);
        assert!(ignored.contains(&"TEXT".to_string()));
        assert!(ignored.contains(&"--size".to_string()));
        // Underscore ids are shown with hyphens, matching the real flag name.
        assert!(ignored.contains(&"--flip-h".to_string()));
        assert!(!ignored.contains(&"--font".to_string()));
        assert!(!ignored.contains(&"--flip-v".to_string()));
    }

    #[test]
    fn test_defaulted_flags_are_not_reported() {
        // Only --layout is given; defaults (font, align, ...) must not warn.
        let matches = print_matches(&["ptouch", "print", "-l", "x.ptl"]);
        let ignored = ignored_content_flags(&matches);
        assert!(
            ignored.is_empty(),
            "unexpected ignored flags: {:?}",
            ignored
        );
    }

    #[test]
    fn test_parse_set_args_ok() {
        let set = vec!["name=Alice".to_string(), "id=A=1".to_string()];
        let values = parse_set_args(&set).unwrap();
        assert_eq!(values.get("name").map(String::as_str), Some("Alice"));
        // Only the first '=' splits, so values may contain '='.
        assert_eq!(values.get("id").map(String::as_str), Some("A=1"));
    }

    #[test]
    fn test_parse_set_args_rejects_missing_eq() {
        assert!(parse_set_args(&["bogus".to_string()]).is_err());
    }

    #[test]
    fn test_validate_vars_missing_errors() {
        let declared = vec!["name".to_string(), "id".to_string()];
        let provided: BTreeSet<String> = ["name".to_string()].into_iter().collect();
        assert!(validate_vars(&declared, &provided, false).is_err());
        // allow_missing turns the error off.
        assert!(validate_vars(&declared, &provided, true).is_ok());
    }

    #[test]
    fn test_validate_vars_unused_is_ok() {
        let declared = vec!["name".to_string()];
        let provided: BTreeSet<String> = ["name".to_string(), "extra".to_string()]
            .into_iter()
            .collect();
        // "extra" is unused (warns) but not an error.
        assert!(validate_vars(&declared, &provided, false).is_ok());
    }

    #[test]
    fn test_build_row_values_row_overrides_base() {
        let mut base = BTreeMap::new();
        base.insert("name".to_string(), "Default".to_string());
        base.insert("dept".to_string(), "Eng".to_string());
        let headers = vec!["name".to_string(), "id".to_string()];
        let record = vec!["Alice".to_string(), "A001".to_string()];
        let values = build_row_values(&base, &headers, &record);
        assert_eq!(values.get("name").map(String::as_str), Some("Alice")); // row wins
        assert_eq!(values.get("id").map(String::as_str), Some("A001")); // from row
        assert_eq!(values.get("dept").map(String::as_str), Some("Eng")); // base kept
    }

    #[test]
    fn test_output_n_token_replacement() {
        assert_eq!("label-{n}.png".replace("{n}", "3"), "label-3.png");
    }

    #[test]
    fn cable_flag_sequence_parses() {
        let cli = Cli::try_parse_from([
            "ptouch",
            "cable",
            "flag",
            "--diameter",
            "6",
            "--prefix",
            "CBL-",
            "--from",
            "1",
            "--count",
            "3",
            "-o",
            "out.png",
        ])
        .unwrap();
        let Commands::Cable(cable) = cli.command else {
            panic!("expected cable command");
        };
        let CableAction::Flag(flag) = cable.action else {
            panic!("expected flag");
        };
        assert_eq!(flag.diameter, 6.0);
        assert_eq!(flag.prefix.as_deref(), Some("CBL-"));
        assert_eq!(flag.count, 3);
        assert_eq!(flag.output.as_deref(), Some("out.png"));
        assert_eq!(
            resolve_cable_texts(&flag).unwrap(),
            vec!["CBL-001", "CBL-002", "CBL-003"]
        );
    }

    #[test]
    fn cable_series_puts_the_number_on_the_chosen_line() {
        let cli = Cli::try_parse_from([
            "ptouch",
            "cable",
            "flag",
            "--prefix",
            "CBL-",
            "--count",
            "2",
            "--line",
            "LAN",
            "--id-line",
            "2",
            "--height",
            "16",
            "--height",
            "30",
        ])
        .unwrap();
        let Commands::Cable(cable) = cli.command else {
            panic!("expected cable command");
        };
        let CableAction::Flag(flag) = cable.action else {
            panic!("expected flag");
        };
        assert_eq!(flag.height, vec![16, 30]);
        assert_eq!(
            resolve_cable_texts(&flag).unwrap(),
            vec!["LAN|CBL-001", "LAN|CBL-002"]
        );
    }

    #[test]
    fn cable_csv_header_uses_columns_as_lines() {
        let path = std::env::temp_dir().join(format!("ptouch-table-{}.csv", std::process::id()));
        std::fs::write(&path, "nombre0,addr\nB1-PR-AF,ADDR: 1\nB1-PR-DV,ADDR: 2\n").unwrap();
        let cli = Cli::try_parse_from([
            "ptouch",
            "cable",
            "flag",
            "--csv",
            path.to_str().unwrap(),
            "--header",
        ])
        .unwrap();
        let Commands::Cable(cable) = cli.command else {
            panic!("expected cable command");
        };
        let CableAction::Flag(flag) = cable.action else {
            panic!("expected flag");
        };
        assert_eq!(
            resolve_cable_texts(&flag).unwrap(),
            vec!["B1-PR-AF|ADDR: 1", "B1-PR-DV|ADDR: 2"]
        );
        std::fs::remove_file(path).ok();
    }
}
