use clap::{CommandFactory, Parser, Subcommand, parser::ValueSource};
use macmon::{App, Sampler, debug};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;

mod serve;

#[derive(Debug, Subcommand)]
enum Commands {
  /// Output metrics in JSON format (suitable for piping)
  #[command(alias = "raw")]
  Pipe {
    /// Number of samples to run for. Set to 0 to run indefinitely
    #[arg(short, long, default_value_t = 0)]
    samples: u32,

    /// Include SoC information in the output
    #[arg(long, default_value_t = false)]
    soc_info: bool,
  },

  /// Serve metrics over HTTP (JSON at /json, Prometheus at /metrics)
  Serve {
    /// Port to listen on
    #[arg(short, long, default_value_t = 9090)]
    port: u16,

    /// Install as a launchd service (auto-start on login)
    #[arg(long, default_value_t = false)]
    install: bool,

    /// Uninstall the launchd service
    #[arg(long, default_value_t = false)]
    uninstall: bool,
  },

  /// Log metrics to a text file every 250ms (file saved next to the executable)
  Log {
    /// Number of samples to run for. Set to 0 to run indefinitely
    #[arg(short, long, default_value_t = 0)]
    samples: u32,
  },

  /// Print debug information
  Debug,
}

/// Sudoless performance monitoring CLI tool for Apple Silicon processors
/// https://github.com/vladkens/macmon
#[derive(Debug, Parser)]
#[command(version, verbatim_doc_comment)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// Update interval in milliseconds
  #[arg(short, long, global = true, default_value_t = 1000)]
  interval: u32,
}

// ── Formatting helpers ────────────────────────────────────────────────────────

const GB: u64 = 1024 * 1024 * 1024;

fn bar(value: f64, max: f64, width: usize) -> String {
  let pct = if max > 0.0 { (value / max).clamp(0.0, 1.0) } else { 0.0 };
  let filled = (pct * width as f64).round() as usize;
  let empty = width - filled;
  format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn fmt_freq(mhz: u32) -> String {
  if mhz == 0 {
    return "    N/A  ".to_string();
  }
  if mhz >= 1000 {
    format!("{:6.2} GHz", mhz as f64 / 1000.0)
  } else {
    format!("{:4} MHz  ", mhz)
  }
}

fn fmt_watts(w: f32) -> String {
  if w == 0.0 { "   N/A".to_string() } else { format!("{:6.2} W", w) }
}

fn fmt_temp(t: f32) -> String {
  if t == 0.0 { "  N/A".to_string() } else { format!("{:5.1}°C", t) }
}

fn fmt_gb(bytes: u64) -> String {
  format!("{:.2} GB", bytes as f64 / GB as f64)
}

// ── Log file path ─────────────────────────────────────────────────────────────
// Named: "<Chip_Name>-<ModelID>-<YYYY-MM-DD_HH-MM-SS>.txt"
// e.g.:  "Apple_M4_Pro-Mac16,6-2026-04-11_14-23-07.txt"
// Saved next to the executable.

fn log_file_path(soc: &macmon::SocInfo) -> PathBuf {
  let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
  let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));

  let now = chrono::Local::now();
  let ts = now.format("%Y-%m-%d_%H-%M-%S").to_string();

  // Spaces → underscores in chip name so the filename stays clean
  let chip = soc.chip_name.replace(' ', "_");

  let filename = format!("{}-{}-{}.txt", chip, soc.mac_model, ts);
  dir.join(filename)
}

// ── Session header (written once at file open) ────────────────────────────────

fn build_session_header(soc: &macmon::SocInfo, log_path: &PathBuf) -> String {
  let now = chrono::Local::now();
  let ts = now.format("%Y-%m-%d %H:%M:%S").to_string();

  let mut L: Vec<String> = Vec::new();
  L.push("╔══════════════════════════════════════════════════════════════╗".into());
  L.push("║              INIZIO SESSIONE — macmon log                    ║".into());
  L.push("╚══════════════════════════════════════════════════════════════╝".into());
  L.push(String::new());
  L.push(format!("  Data/ora inizio  : {}", ts));
  L.push(format!("  File di log      : {}", log_path.display()));
  L.push(format!("  Intervallo       : 250 ms"));
  L.push(String::new());
  L.push("  ── INFORMAZIONI MACCHINA ─────────────────────────────────────".into());
  L.push(format!("  Chip             : {}", soc.chip_name));
  L.push(format!("  Model ID         : {}", soc.mac_model));
  L.push(format!("  Memoria          : {} GB", soc.memory_gb));
  L.push(format!(
    "  Core CPU         : {} {}-core + {} {}-core  (tot. {})",
    soc.ecpu_cores,
    soc.ecpu_label,
    soc.pcpu_cores,
    soc.pcpu_label,
    soc.ecpu_cores as u16 + soc.pcpu_cores as u16,
  ));
  L.push(format!("  Core GPU         : {}", soc.gpu_cores));
  L.push(String::new());
  L.push("  ── RANGE FREQUENZE DISPONIBILI ───────────────────────────────".into());
  L.push(format!(
    "  {}-Core           : {} – {} MHz",
    soc.ecpu_label,
    soc.ecpu_freqs.first().copied().unwrap_or(0),
    soc.ecpu_freqs.last().copied().unwrap_or(0),
  ));
  L.push(format!(
    "  {}-Core           : {} – {} MHz",
    soc.pcpu_label,
    soc.pcpu_freqs.first().copied().unwrap_or(0),
    soc.pcpu_freqs.last().copied().unwrap_or(0),
  ));
  L.push(format!(
    "  GPU              : {} – {} MHz",
    soc.gpu_freqs.first().copied().unwrap_or(0),
    soc.gpu_freqs.last().copied().unwrap_or(0),
  ));
  L.push(String::new());
  L.push("═══════════════════════════════════════════════════════════════".into());
  L.push(String::new());
  L.join("\n")
}

// ── Per-sample entry ──────────────────────────────────────────────────────────

fn build_entry(metrics: &macmon::Metrics, soc: &macmon::SocInfo, index: u64) -> String {
  let now = chrono::Local::now();
  // Millisecond-precision timestamp so 250ms samples are distinguishable
  let ts = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();

  let mem = &metrics.memory;
  let ram_pct = if mem.ram_total > 0 {
    mem.ram_usage as f64 / mem.ram_total as f64 * 100.0
  } else {
    0.0
  };
  let swap_pct = if mem.swap_total > 0 {
    mem.swap_usage as f64 / mem.swap_total as f64 * 100.0
  } else {
    0.0
  };

  let ecpu_pct = metrics.ecpu_usage.1 as f64 * 100.0;
  let pcpu_pct = metrics.pcpu_usage.1 as f64 * 100.0;
  let gpu_pct  = metrics.gpu_usage.1  as f64 * 100.0;
  let cpu_pct  = metrics.cpu_usage_pct as f64 * 100.0;

  let max_ecpu = *soc.ecpu_freqs.last().unwrap_or(&1) as f64;
  let max_pcpu = *soc.pcpu_freqs.last().unwrap_or(&1) as f64;
  let max_gpu  = *soc.gpu_freqs.last().unwrap_or(&1)  as f64;

  let mut L: Vec<String> = Vec::new();
  L.push("─────────────────────────────────────────────────────────────".into());
  L.push(format!("  Campione #{:<6}  {}", index, ts));
  L.push("─────────────────────────────────────────────────────────────".into());
  L.push(String::new());

  // CPU overview
  L.push(format!(
    "  CPU  utilizzo combinato  {:5.1}%  {}",
    cpu_pct,
    bar(cpu_pct, 100.0, 20)
  ));
  L.push(format!(
    "    {}-Core  {:5.1}%  {}   {} / {}",
    soc.ecpu_label,
    ecpu_pct,
    bar(metrics.ecpu_usage.0 as f64, max_ecpu, 16),
    fmt_freq(metrics.ecpu_usage.0),
    fmt_freq(*soc.ecpu_freqs.last().unwrap_or(&0)),
  ));
  L.push(format!(
    "    {}-Core  {:5.1}%  {}   {} / {}",
    soc.pcpu_label,
    pcpu_pct,
    bar(metrics.pcpu_usage.0 as f64, max_pcpu, 16),
    fmt_freq(metrics.pcpu_usage.0),
    fmt_freq(*soc.pcpu_freqs.last().unwrap_or(&0)),
  ));
  L.push(String::new());

  // GPU
  L.push(format!(
    "  GPU  {:5.1}%  {}   {} / {}",
    gpu_pct,
    bar(metrics.gpu_usage.0 as f64, max_gpu, 20),
    fmt_freq(metrics.gpu_usage.0),
    fmt_freq(*soc.gpu_freqs.last().unwrap_or(&0)),
  ));
  L.push(String::new());

  // Power
  L.push("  CONSUMI".into());
  L.push(format!("    CPU              : {}", fmt_watts(metrics.cpu_power)));
  L.push(format!("    GPU              : {}", fmt_watts(metrics.gpu_power)));
  L.push(format!("    ANE              : {}", fmt_watts(metrics.ane_power)));
  L.push(format!("    SoC totale       : {}", fmt_watts(metrics.all_power)));
  if metrics.sys_power > 0.0 {
    L.push(format!("    Sistema (PSTR)   : {}", fmt_watts(metrics.sys_power)));
  }
  L.push(String::new());

  // Temperature
  L.push("  TEMPERATURE".into());
  L.push(format!("    CPU avg          : {}", fmt_temp(metrics.temp.cpu_temp_avg)));
  L.push(format!("    GPU avg          : {}", fmt_temp(metrics.temp.gpu_temp_avg)));
  L.push(String::new());

  // Memory
  L.push("  MEMORIA".into());
  L.push(format!(
    "    RAM   {}  /  {}  ({:5.1}%)  {}",
    fmt_gb(mem.ram_usage),
    fmt_gb(mem.ram_total),
    ram_pct,
    bar(ram_pct, 100.0, 16),
  ));
  if mem.swap_total > 0 {
    L.push(format!(
      "    SWAP  {}  /  {}  ({:5.1}%)  {}",
      fmt_gb(mem.swap_usage),
      fmt_gb(mem.swap_total),
      swap_pct,
      bar(swap_pct, 100.0, 16),
    ));
  }
  L.push(String::new());

  L.join("\n")
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn Error>> {
  let args = Cli::parse();

  match &args.command {
    Some(Commands::Pipe { samples, soc_info }) => {
      let mut sampler = Sampler::new()?;
      let mut counter = 0u32;

      let soc_info_val = if *soc_info { Some(sampler.get_soc_info().clone()) } else { None };

      loop {
        let doc = sampler.get_metrics(args.interval.max(100))?;

        let mut doc = serde_json::to_value(&doc)?;
        if let Some(ref soc) = soc_info_val {
          doc["soc"] = serde_json::to_value(soc)?;
        }
        doc["timestamp"] = serde_json::to_value(chrono::Utc::now().to_rfc3339())?;
        let doc = serde_json::to_string(&doc)?;

        println!("{}", doc);

        counter += 1;
        if *samples > 0 && counter >= *samples {
          break;
        }
      }
    }

    Some(Commands::Serve { port, install, uninstall }) => {
      if *install || *uninstall {
        serve::launchd(*port, *install)?;
        return Ok(());
      }
      let mut sampler = Sampler::new()?;
      let soc = Arc::new(sampler.get_soc_info().clone());
      let shared: serve::SharedMetrics = Arc::new(RwLock::new(None));

      let shared_http = Arc::clone(&shared);
      let soc_http = Arc::clone(&soc);
      let port = *port;
      thread::spawn(move || {
        if let Err(e) = serve::run(port, shared_http, soc_http) {
          eprintln!("server error: {e}");
        }
      });

      loop {
        match sampler.get_metrics(args.interval.max(100)) {
          Ok(m) => *shared.write().unwrap() = Some(m),
          Err(e) => eprintln!("sampling error: {e}"),
        }
      }
    }

    Some(Commands::Log { samples }) => {
      let mut sampler = Sampler::new()?;
      let soc = sampler.get_soc_info().clone();
      let log_path = log_file_path(&soc);

      // Write session header (create/truncate)
      {
        let mut file = OpenOptions::new()
          .create(true)
          .write(true)
          .truncate(true)
          .open(&log_path)?;
        file.write_all(build_session_header(&soc, &log_path).as_bytes())?;
        file.flush()?;
      }

      println!("╔══════════════════════════════════════════════════════════════╗");
      println!("║               macmon — modalità log                         ║");
      println!("╚══════════════════════════════════════════════════════════════╝");
      println!("  Chip     : {}", soc.chip_name);
      println!("  Model ID : {}", soc.mac_model);
      println!("  Log      : {}", log_path.display());
      println!("  Campione ogni 250 ms — Ctrl+C per fermare\n");

      let mut counter = 0u64;

      loop {
        let metrics = sampler.get_metrics(250)?;
        counter += 1;

        // Append sample to file
        {
          let mut file = OpenOptions::new().append(true).open(&log_path)?;
          file.write_all(build_entry(&metrics, &soc, counter).as_bytes())?;
          file.flush()?;
        }

        // Compact terminal summary
        println!(
          "[#{:06}] CPU {:4.1}%  {}-Core {:4.1}% {}  {}-Core {:4.1}% {}  GPU {:4.1}% {}  {:.2}W  CPU {}/GPU {}",
          counter,
          metrics.cpu_usage_pct * 100.0,
          soc.ecpu_label,
          metrics.ecpu_usage.1 * 100.0,
          fmt_freq(metrics.ecpu_usage.0),
          soc.pcpu_label,
          metrics.pcpu_usage.1 * 100.0,
          fmt_freq(metrics.pcpu_usage.0),
          metrics.gpu_usage.1 * 100.0,
          fmt_freq(metrics.gpu_usage.0),
          metrics.all_power,
          fmt_temp(metrics.temp.cpu_temp_avg),
          fmt_temp(metrics.temp.gpu_temp_avg),
        );

        if *samples > 0 && counter >= *samples as u64 {
          break;
        }
      }

      println!("\nFine sessione. Log salvato in: {}", log_path.display());
    }

    Some(Commands::Debug) => debug::print_debug()?,

    _ => {
      let mut app = App::new()?;

      let matches = Cli::command().get_matches();
      let msec = match matches.value_source("interval") {
        Some(ValueSource::CommandLine) => Some(args.interval),
        _ => None,
      };

      app.run_loop(msec)?;
    }
  }

  Ok(())
}
