use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::{io::stdout, time::Instant};
use std::{sync::mpsc, time::Duration};

use ratatui::crossterm::{
  ExecutableCommand,
  event::{self, KeyCode, KeyModifiers},
  terminal,
};
use ratatui::{prelude::*, widgets::*};

use crate::config::{Config, ViewType};
use crate::metrics::{Metrics, Sampler, zero_div};
use crate::{metrics::MemMetrics, sources::SocInfo};

type WithError<T> = Result<T, Box<dyn std::error::Error>>;

// MARK: Log writer

const LOG_GB: u64 = 1024 * 1024 * 1024;

fn log_bar(value: f64, max: f64, width: usize) -> String {
  let pct = if max > 0.0 { (value / max).clamp(0.0, 1.0) } else { 0.0 };
  let filled = (pct * width as f64).round() as usize;
  format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn log_fmt_freq(mhz: u32) -> String {
  format!("{:5} MHz", mhz)
}

fn log_fmt_watts(w: f32) -> String {
  format!("{:6.2} W", w)
}

fn log_fmt_temp(t: f32) -> String {
  if t == 0.0 { "  N/A".to_string() } else { format!("{:5.1}°C", t) }
}

fn log_fmt_gb(bytes: u64) -> String {
  format!("{:.2} GB", bytes as f64 / LOG_GB as f64)
}

fn log_file_path(soc: &SocInfo) -> PathBuf {
  let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
  let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
  let now = chrono::Local::now();
  let ts = now.format("%Y-%m-%d_%H-%M-%S").to_string();
  let chip = soc.chip_name.replace(' ', "_");
  dir.join(format!("{}-{}-{}.txt", chip, soc.mac_model, ts))
}

fn log_session_header(soc: &SocInfo, path: &PathBuf) -> String {
  let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
  let mut L: Vec<String> = Vec::new();
  L.push("╔══════════════════════════════════════════════════════════════╗".into());
  L.push("║              INIZIO SESSIONE — macmon log                    ║".into());
  L.push("╚══════════════════════════════════════════════════════════════╝".into());
  L.push(String::new());
  L.push(format!("  Data/ora inizio  : {}", ts));
  L.push(format!("  File di log      : {}", path.display()));
  L.push(format!("  Intervallo       : segue UI (ms variabili)"));
  L.push(String::new());
  L.push("  ── INFORMAZIONI MACCHINA ─────────────────────────────────────".into());
  L.push(format!("  Chip             : {}", soc.chip_name));
  L.push(format!("  Model ID         : {}", soc.mac_model));
  L.push(format!("  Memoria          : {} GB", soc.memory_gb));
  L.push(format!(
    "  Core CPU         : {} {}-core + {} {}-core  (tot. {})",
    soc.ecpu_cores, soc.ecpu_label,
    soc.pcpu_cores, soc.pcpu_label,
    soc.ecpu_cores as u16 + soc.pcpu_cores as u16,
  ));
  L.push(format!("  Core GPU         : {}", soc.gpu_cores));
  L.push(String::new());
  L.push("  ── RANGE FREQUENZE DISPONIBILI ───────────────────────────────".into());
  L.push(format!("  {}-Core  : {} MHz",
    soc.ecpu_label,
    soc.ecpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(format!("  {}-Core  : {} MHz",
    soc.pcpu_label,
    soc.pcpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(format!("  GPU     : {} MHz",
    soc.gpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(String::new());
  L.push("═══════════════════════════════════════════════════════════════".into());
  L.push(String::new());
  L.join("\n")
}

fn log_build_entry(metrics: &Metrics, soc: &SocInfo, index: u64) -> String {
  let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
  let mem = &metrics.memory;
  let ram_pct  = if mem.ram_total  > 0 { mem.ram_usage  as f64 / mem.ram_total  as f64 * 100.0 } else { 0.0 };
  let swap_pct = if mem.swap_total > 0 { mem.swap_usage as f64 / mem.swap_total as f64 * 100.0 } else { 0.0 };
  let ecpu_pct = metrics.ecpu_usage.1 as f64 * 100.0;
  let pcpu_pct = metrics.pcpu_usage.1 as f64 * 100.0;
  let gpu_pct  = metrics.gpu_usage.1  as f64 * 100.0;
  let cpu_pct  = metrics.cpu_usage_pct as f64 * 100.0;
  let max_ecpu = *soc.ecpu_freqs.last().unwrap_or(&1) as f64;
  let max_pcpu = *soc.pcpu_freqs.last().unwrap_or(&1) as f64;
  let max_gpu  = *soc.gpu_freqs.last().unwrap_or(&1)  as f64;

  // Tutte le barre hanno la stessa larghezza (20) e i valori sono padding fisso
  // Layout colonne: "    X-Core  PPP.P%  [████████████████████]   F.FF GHz / F.FF GHz"
  let B = 20; // larghezza barra unica per tutto il log

  let mut L: Vec<String> = Vec::new();
  L.push(String::new());
  L.push(String::new());
  L.push("─────────────────────────────────────────────────────────────".into());
  L.push(format!("  Campione #{:<6}  {}", index, ts));
  L.push("─────────────────────────────────────────────────────────────".into());
  L.push(String::new());

  // UTILIZZO — etichetta fissa 16 caratteri, poi %6.1f%, poi barra 20, poi frequenze
  // "  CPU combinato   " = 18 char
  // "  CPU E-Core      " = 18 char
  // "  CPU P-Core      " = 18 char
  // "  GPU             " = 18 char
  let ecpu_lbl = format!("CPU {}-Core", soc.ecpu_label);
  let pcpu_lbl = format!("CPU {}-Core", soc.pcpu_label);

  L.push("  UTILIZZO".into());
  L.push(format!("    {:<16} {:5.1}%  {}",
    "CPU combinato", cpu_pct, log_bar(cpu_pct, 100.0, B)));
  L.push(String::new());
  L.push(format!("    {:<16} {:5.1}%  {}   {} / {}",
    ecpu_lbl, ecpu_pct,
    log_bar(metrics.ecpu_usage.0 as f64, max_ecpu, B),
    log_fmt_freq(metrics.ecpu_usage.0),
    log_fmt_freq(*soc.ecpu_freqs.last().unwrap_or(&0))));
  L.push(format!("    {:<16} {:5.1}%  {}   {} / {}",
    pcpu_lbl, pcpu_pct,
    log_bar(metrics.pcpu_usage.0 as f64, max_pcpu, B),
    log_fmt_freq(metrics.pcpu_usage.0),
    log_fmt_freq(*soc.pcpu_freqs.last().unwrap_or(&0))));
  L.push(String::new());
  L.push(format!("    {:<16} {:5.1}%  {}   {} / {}",
    "GPU", gpu_pct,
    log_bar(metrics.gpu_usage.0 as f64, max_gpu, B),
    log_fmt_freq(metrics.gpu_usage.0),
    log_fmt_freq(*soc.gpu_freqs.last().unwrap_or(&0))));
  L.push(String::new());

  // Consumi
  L.push("  CONSUMI".into());
  L.push(format!("    CPU              : {}", log_fmt_watts(metrics.cpu_power)));
  L.push(format!("    GPU              : {}", log_fmt_watts(metrics.gpu_power)));
  L.push(format!("    ANE              : {}", log_fmt_watts(metrics.ane_power)));
  L.push(format!("    SoC totale       : {}", log_fmt_watts(metrics.all_power)));
  if metrics.sys_power > 0.0 {
    L.push(format!("    Sistema (PSTR)   : {}", log_fmt_watts(metrics.sys_power)));
  }
  L.push(String::new());

  // Temperature
  L.push("  TEMPERATURE".into());
  L.push(format!("    CPU avg          : {}", log_fmt_temp(metrics.temp.cpu_temp_avg)));
  L.push(format!("    GPU avg          : {}", log_fmt_temp(metrics.temp.gpu_temp_avg)));
  L.push(String::new());

  // Memoria
  L.push("  MEMORIA".into());
  L.push(format!("    RAM   {:7}  /  {:7}  ({:5.1}%)  {}",
    log_fmt_gb(mem.ram_usage), log_fmt_gb(mem.ram_total), ram_pct, log_bar(ram_pct, 100.0, B)));
  if mem.swap_total > 0 {
    L.push(format!("    SWAP  {:7}  /  {:7}  ({:5.1}%)  {}",
      log_fmt_gb(mem.swap_usage), log_fmt_gb(mem.swap_total), swap_pct, log_bar(swap_pct, 100.0, B)));
  }
  L.push(String::new());

  L.join("\n")
}

// MARK: Peak stats tracker

#[derive(Debug, Default)]
struct PeakStats {
  cpu_power: f32,
  gpu_power: f32,
  ane_power: f32,
  all_power: f32,
  sys_power: f32,
  cpu_temp: f32,
  gpu_temp: f32,
  ecpu_freq: u32,
  pcpu_freq: u32,
  gpu_freq: u32,
  cpu_usage: f32,
  ram_usage: u64,
  // accumulatori per le medie
  sum_cpu_power: f64,
  sum_gpu_power: f64,
  sum_ane_power: f64,
  sum_all_power: f64,
  sum_sys_power: f64,
  count: u64,
}

impl PeakStats {
  fn update(&mut self, m: &Metrics) {
    if m.cpu_power  > self.cpu_power  { self.cpu_power  = m.cpu_power }
    if m.gpu_power  > self.gpu_power  { self.gpu_power  = m.gpu_power }
    if m.ane_power  > self.ane_power  { self.ane_power  = m.ane_power }
    if m.all_power  > self.all_power  { self.all_power  = m.all_power }
    if m.sys_power  > self.sys_power  { self.sys_power  = m.sys_power }
    if m.temp.cpu_temp_avg > self.cpu_temp { self.cpu_temp = m.temp.cpu_temp_avg }
    if m.temp.gpu_temp_avg > self.gpu_temp { self.gpu_temp = m.temp.gpu_temp_avg }
    if m.ecpu_usage.0 > self.ecpu_freq { self.ecpu_freq = m.ecpu_usage.0 }
    if m.pcpu_usage.0 > self.pcpu_freq { self.pcpu_freq = m.pcpu_usage.0 }
    if m.gpu_usage.0  > self.gpu_freq  { self.gpu_freq  = m.gpu_usage.0 }
    if m.cpu_usage_pct > self.cpu_usage { self.cpu_usage = m.cpu_usage_pct }
    if m.memory.ram_usage > self.ram_usage { self.ram_usage = m.memory.ram_usage }
    // medie
    self.sum_cpu_power += m.cpu_power as f64;
    self.sum_gpu_power += m.gpu_power as f64;
    self.sum_ane_power += m.ane_power as f64;
    self.sum_all_power += m.all_power as f64;
    self.sum_sys_power += m.sys_power as f64;
    self.count += 1;
  }

  fn avg_cpu_power(&self) -> f32 { if self.count > 0 { (self.sum_cpu_power / self.count as f64) as f32 } else { 0.0 } }
  fn avg_gpu_power(&self) -> f32 { if self.count > 0 { (self.sum_gpu_power / self.count as f64) as f32 } else { 0.0 } }
  fn avg_ane_power(&self) -> f32 { if self.count > 0 { (self.sum_ane_power / self.count as f64) as f32 } else { 0.0 } }
  fn avg_all_power(&self) -> f32 { if self.count > 0 { (self.sum_all_power / self.count as f64) as f32 } else { 0.0 } }
  fn avg_sys_power(&self) -> f32 { if self.count > 0 { (self.sum_sys_power / self.count as f64) as f32 } else { 0.0 } }
}

fn log_session_footer(soc: &SocInfo, path: &PathBuf, samples: u64, peaks: &PeakStats, start: &chrono::DateTime<chrono::Local>) -> String {
  let now = chrono::Local::now();
  let ts = now.format("%Y-%m-%d %H:%M:%S").to_string();
  let elapsed = now.signed_duration_since(*start);
  let h = elapsed.num_hours();
  let m = elapsed.num_minutes() % 60;
  let s = elapsed.num_seconds() % 60;
  let duration_str = if h > 0 {
    format!("{}h {:02}m {:02}s", h, m, s)
  } else if m > 0 {
    format!("{}m {:02}s", m, s)
  } else {
    format!("{}s", s)
  };

  let mut L: Vec<String> = Vec::new();
  L.push(String::new());
  L.push(String::new());
  L.push("╔══════════════════════════════════════════════════════════════╗".into());
  L.push("║               FINE SESSIONE — macmon log                    ║".into());
  L.push("╚══════════════════════════════════════════════════════════════╝".into());
  L.push(String::new());
  L.push(format!("  Data/ora fine     : {}", ts));
  L.push(format!("  Durata sessione   : {}", duration_str));
  L.push(format!("  File di log       : {}", path.display()));
  L.push(format!("  Intervallo        : segue UI (ms variabili)"));
  L.push(format!("  Campioni totali   : {}", samples));
  L.push(String::new());
  L.push("  ── INFORMAZIONI MACCHINA ─────────────────────────────────────".into());
  L.push(format!("  Chip              : {}", soc.chip_name));
  L.push(format!("  Model ID          : {}", soc.mac_model));
  L.push(format!("  Memoria           : {} GB", soc.memory_gb));
  L.push(format!(
    "  Core CPU          : {} {}-core + {} {}-core  (tot. {})",
    soc.ecpu_cores, soc.ecpu_label,
    soc.pcpu_cores, soc.pcpu_label,
    soc.ecpu_cores as u16 + soc.pcpu_cores as u16,
  ));
  L.push(format!("  Core GPU          : {}", soc.gpu_cores));
  L.push(String::new());
  L.push("  ── RANGE FREQUENZE DISPONIBILI ───────────────────────────────".into());
  L.push(format!("  {}-Core  : {} MHz",
    soc.ecpu_label,
    soc.ecpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(format!("  {}-Core  : {} MHz",
    soc.pcpu_label,
    soc.pcpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(format!("  GPU     : {} MHz",
    soc.gpu_freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" · ")));
  L.push(String::new());
  L.push("  ── PICCHI RILEVATI DURANTE LA SESSIONE ───────────────────────".into());
  L.push(format!("  CPU utilizzo max  : {:5.1}%", peaks.cpu_usage * 100.0));
  L.push(format!("  {}-Core freq max   : {}", soc.ecpu_label, log_fmt_freq(peaks.ecpu_freq)));
  L.push(format!("  {}-Core freq max   : {}", soc.pcpu_label, log_fmt_freq(peaks.pcpu_freq)));
  L.push(format!("  GPU freq max      : {}", log_fmt_freq(peaks.gpu_freq)));
  L.push(String::new());
  L.push(format!("  CPU power max     : {}", log_fmt_watts(peaks.cpu_power)));
  L.push(format!("  GPU power max     : {}", log_fmt_watts(peaks.gpu_power)));
  L.push(format!("  ANE power max     : {}", log_fmt_watts(peaks.ane_power)));
  L.push(format!("  SoC totale max    : {}", log_fmt_watts(peaks.all_power)));
  if peaks.sys_power > 0.0 {
    L.push(format!("  Sistema max       : {}", log_fmt_watts(peaks.sys_power)));
  }
  L.push(String::new());
  L.push(format!("  CPU temp max      : {}", log_fmt_temp(peaks.cpu_temp)));
  L.push(format!("  GPU temp max      : {}", log_fmt_temp(peaks.gpu_temp)));
  L.push(String::new());
  L.push(format!("  RAM max           : {}", log_fmt_gb(peaks.ram_usage)));
  L.push(String::new());
  L.push("  ── MEDIE DURANTE LA SESSIONE ─────────────────────────────────".into());
  L.push(format!("  CPU power avg     : {}", log_fmt_watts(peaks.avg_cpu_power())));
  L.push(format!("  GPU power avg     : {}", log_fmt_watts(peaks.avg_gpu_power())));
  L.push(format!("  ANE power avg     : {}", log_fmt_watts(peaks.avg_ane_power())));
  L.push(format!("  SoC totale avg    : {}", log_fmt_watts(peaks.avg_all_power())));
  if peaks.sys_power > 0.0 {
    L.push(format!("  Sistema avg       : {}", log_fmt_watts(peaks.avg_sys_power())));
  }
  L.push(String::new());
  L.push("═══════════════════════════════════════════════════════════════".into());
  L.push(String::new());
  L.join("\n")
}



const GB: u64 = 1024 * 1024 * 1024;
const MAX_SPARKLINE: usize = 128;
const MAX_TEMPS: usize = 8;

// MARK: Term utils

fn enter_term() -> Terminal<impl Backend> {
  std::panic::set_hook(Box::new(|info| {
    leave_term();
    eprintln!("{}", info);
  }));

  terminal::enable_raw_mode().unwrap();
  stdout().execute(terminal::EnterAlternateScreen).unwrap();

  let term = CrosstermBackend::new(std::io::stdout());
  Terminal::new(term).unwrap()
}

fn leave_term() {
  terminal::disable_raw_mode().unwrap();
  stdout().execute(terminal::LeaveAlternateScreen).unwrap();
}

// MARK: Storage

#[derive(Debug, Default)]
struct FreqStore {
  items: Vec<u64>, // from 0 to 100
  top_value: u64,
  usage: f64, // from 0.0 to 1.0
}

impl FreqStore {
  fn push(&mut self, value: u64, usage: f64) {
    self.items.insert(0, (usage * 100.0) as u64);
    self.items.truncate(MAX_SPARKLINE);

    self.top_value = value;
    self.usage = usage;
  }
}

#[derive(Debug, Default)]
struct PowerStore {
  items: Vec<u64>,
  top_value: f64,
  max_value: f64,
  avg_value: f64,
}

impl PowerStore {
  fn push(&mut self, value: f64) {
    let was_top = if !self.items.is_empty() { self.items[0] as f64 / 1000.0 } else { 0.0 };

    self.items.insert(0, (value * 1000.0) as u64);
    self.items.truncate(MAX_SPARKLINE);

    self.top_value = avg2(was_top, value);
    self.avg_value = self.items.iter().sum::<u64>() as f64 / self.items.len() as f64 / 1000.0;
    self.max_value = self.items.iter().max().map_or(0, |v| *v) as f64 / 1000.0;
  }
}

#[derive(Debug, Default)]
struct MemoryStore {
  items: Vec<u64>,
  ram_usage: u64,
  ram_total: u64,
  swap_usage: u64,
  swap_total: u64,
  max_ram: u64,
}

impl MemoryStore {
  fn push(&mut self, value: MemMetrics) {
    self.items.insert(0, value.ram_usage);
    self.items.truncate(MAX_SPARKLINE);

    self.ram_usage = value.ram_usage;
    self.ram_total = value.ram_total;
    self.swap_usage = value.swap_usage;
    self.swap_total = value.swap_total;
    self.max_ram = self.items.iter().max().map_or(0, |v| *v);
  }
}

#[derive(Debug, Default)]
struct TempStore {
  items: Vec<f32>,
}

impl TempStore {
  fn last(&self) -> f32 {
    *self.items.first().unwrap_or(&0.0)
  }

  fn push(&mut self, value: f32) {
    // https://www.tunabellysoftware.com/blog/files/tg-pro-apple-silicon-m3-series-support.html
    // https://github.com/vladkens/macmon/issues/12
    let value = if value == 0.0 { self.trend_ema(0.8) } else { value };
    if value == 0.0 {
      return; // skip if not sensor available
    }

    self.items.insert(0, value);
    self.items.truncate(MAX_TEMPS);
  }

  // https://en.wikipedia.org/wiki/Exponential_smoothing
  fn trend_ema(&self, alpha: f32) -> f32 {
    if self.items.len() < 2 {
      return 0.0;
    }

    // starts from most recent value, so need to be reversed
    let mut iter = self.items.iter().rev();
    let mut ema = *iter.next().unwrap_or(&0.0);

    for &item in iter {
      ema = alpha * item + (1.0 - alpha) * ema;
    }

    ema
  }
}

// MARK: Components

fn h_stack(area: Rect) -> (Rect, Rect) {
  let ha = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Fill(1), Constraint::Fill(1)].as_ref())
    .split(area);

  (ha[0], ha[1])
}

// MARK: Threads

enum Event {
  Update(Metrics),
  ChangeColor,
  ChangeView,
  IncInterval,
  DecInterval,
  Tick,
  Quit,
}

fn handle_key_event(key: &event::KeyEvent, tx: &mpsc::Sender<Event>) -> WithError<()> {
  match key.code {
    KeyCode::Char('q') => Ok(tx.send(Event::Quit)?),
    KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Ok(tx.send(Event::Quit)?),
    KeyCode::Char('c') => Ok(tx.send(Event::ChangeColor)?),
    KeyCode::Char('v') => Ok(tx.send(Event::ChangeView)?),
    KeyCode::Char('+') => Ok(tx.send(Event::IncInterval)?),
    KeyCode::Char('=') => Ok(tx.send(Event::IncInterval)?), // fallback to press without shift
    KeyCode::Char('-') => Ok(tx.send(Event::DecInterval)?),
    _ => Ok(()),
  }
}

fn run_inputs_thread(tx: mpsc::Sender<Event>, tick: u64) {
  let tick_rate = Duration::from_millis(tick);

  std::thread::spawn(move || {
    let mut last_tick = Instant::now();

    loop {
      if event::poll(Duration::from_millis(tick)).unwrap() {
        match event::read().unwrap() {
          event::Event::Key(key) => handle_key_event(&key, &tx).unwrap(),
          _ => {}
        };
      }

      if last_tick.elapsed() >= tick_rate {
        tx.send(Event::Tick).unwrap();
        last_tick = Instant::now();
      }
    }
  });
}

fn run_sampler_thread(tx: mpsc::Sender<Event>, msec: Arc<RwLock<u32>>) {
  std::thread::spawn(move || {
    let mut sampler = Sampler::new().unwrap();

    // Send initial metrics
    tx.send(Event::Update(sampler.get_metrics(100).unwrap())).unwrap();

    loop {
      let msec = *msec.read().unwrap();
      tx.send(Event::Update(sampler.get_metrics(msec).unwrap())).unwrap();
    }
  });
}

// get average of two values, used to smooth out metrics
// see: https://github.com/vladkens/macmon/issues/10
fn avg2<T: num_traits::Float>(a: T, b: T) -> T {
  if a == T::zero() { b } else { (a + b) / T::from(2.0).unwrap() }
}

// MARK: App

#[derive(Debug, Default)]
pub struct App {
  cfg: Config,

  soc: SocInfo,
  mem: MemoryStore,

  cpu_power: PowerStore,
  gpu_power: PowerStore,
  ane_power: PowerStore,
  all_power: PowerStore,
  sys_power: PowerStore,

  cpu_temp: TempStore,
  gpu_temp: TempStore,

  ecpu_freq: FreqStore,
  pcpu_freq: FreqStore,
  igpu_freq: FreqStore,

  log_path: Option<PathBuf>,
  log_counter: u64,
  peaks: PeakStats,
  start_time: Option<chrono::DateTime<chrono::Local>>,
}

impl App {
  pub fn new() -> WithError<Self> {
    let soc = SocInfo::new()?;
    let cfg = Config::load();

    // Inizializza il file di log accanto all'eseguibile
    let log_path = log_file_path(&soc);
    {
      let mut file = OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&log_path)?;
      file.write_all(log_session_header(&soc, &log_path).as_bytes())?;
      file.flush()?;
    }

    Ok(Self { cfg, soc, log_path: Some(log_path), start_time: Some(chrono::Local::now()), ..Default::default() })
  }

  fn update_metrics(&mut self, data: Metrics) {
    // Aggiorna picchi e scrivi log (prima delle push che consumano data)
    self.peaks.update(&data);
    self.log_counter += 1;
    if let Some(ref path) = self.log_path {
      let entry = log_build_entry(&data, &self.soc, self.log_counter);
      if let Ok(mut file) = OpenOptions::new().append(true).open(path) {
        let _ = file.write_all(entry.as_bytes());
        let _ = file.flush();
      }
    }

    self.cpu_power.push(data.cpu_power as f64);
    self.gpu_power.push(data.gpu_power as f64);
    self.ane_power.push(data.ane_power as f64);
    self.all_power.push(data.all_power as f64);
    self.sys_power.push(data.sys_power as f64);
    self.ecpu_freq.push(data.ecpu_usage.0 as u64, data.ecpu_usage.1 as f64);
    self.pcpu_freq.push(data.pcpu_usage.0 as u64, data.pcpu_usage.1 as f64);
    self.igpu_freq.push(data.gpu_usage.0 as u64, data.gpu_usage.1 as f64);

    self.cpu_temp.push(data.temp.cpu_temp_avg);
    self.gpu_temp.push(data.temp.gpu_temp_avg);

    self.mem.push(data.memory);
  }

  fn title_block<'a>(&self, label_l: &str, label_r: &str) -> Block<'a> {
    let mut block = Block::new()
      .borders(Borders::ALL)
      .border_type(BorderType::Rounded)
      .border_style(self.cfg.color)
      // .title_style(Style::default().gray())
      .padding(Padding::ZERO);

    if !label_l.is_empty() {
      block = block.title_top(Line::from(format!(" {label_l} ")));
    }

    if !label_r.is_empty() {
      block = block.title_top(Line::from(format!(" {label_r} ")).alignment(Alignment::Right));
    }

    block
  }

  fn get_power_block<'a>(&self, label: &str, val: &'a PowerStore, temp: f32) -> Sparkline<'a> {
    let label_l = format!(
      "{} {:.2}W ({:.2}, {:.2})",
      // "{} {:.2}W (avg: {:.2}W, max: {:.2}W)",
      // "{} {:.2}W (~{:.2}W ^{:.2}W)",
      label,
      val.top_value,
      val.avg_value,
      val.max_value
    );

    let label_r = if temp > 0.0 { format!("{:.1}°C", temp) } else { "".to_string() };

    Sparkline::default()
      .block(self.title_block(label_l.as_str(), label_r.as_str()))
      .direction(RenderDirection::RightToLeft)
      .data(&val.items)
      .style(self.cfg.color)
  }

  fn render_freq_block(&self, f: &mut Frame, r: Rect, label: &str, val: &FreqStore) {
    let label = format!("{} {:3.0}% @ {:4.0} MHz", label, val.usage * 100.0, val.top_value);
    let block = self.title_block(label.as_str(), "");

    match self.cfg.view_type {
      ViewType::Sparkline => {
        let w = Sparkline::default()
          .block(block)
          .direction(RenderDirection::RightToLeft)
          .data(&val.items)
          .max(100)
          .style(self.cfg.color);
        f.render_widget(w, r);
      }
      ViewType::Gauge => {
        let w = Gauge::default()
          .block(block)
          .gauge_style(self.cfg.color)
          .style(self.cfg.color)
          .label("")
          .ratio(val.usage);
        f.render_widget(w, r);
      }
    }
  }

  fn render_mem_block(&self, f: &mut Frame, r: Rect, val: &MemoryStore) {
    let ram_usage_gb = val.ram_usage as f64 / GB as f64;
    let ram_total_gb = val.ram_total as f64 / GB as f64;

    let swap_usage_gb = val.swap_usage as f64 / GB as f64;
    let swap_total_gb = val.swap_total as f64 / GB as f64;

    let ram_pct = zero_div(ram_usage_gb, ram_total_gb) * 100.0;
    let label_l = format!("RAM {:4.2} / {:4.1} GB ({:.1}%)", ram_usage_gb, ram_total_gb, ram_pct);
    let label_r = format!("SWAP {:.2} / {:.1} GB", swap_usage_gb, swap_total_gb);

    let block = self.title_block(label_l.as_str(), label_r.as_str());
    match self.cfg.view_type {
      ViewType::Sparkline => {
        let w = Sparkline::default()
          .block(block)
          .direction(RenderDirection::RightToLeft)
          .data(&val.items)
          .max(val.ram_total)
          .style(self.cfg.color);
        f.render_widget(w, r);
      }
      ViewType::Gauge => {
        let w = Gauge::default()
          .block(block)
          .gauge_style(self.cfg.color)
          .style(self.cfg.color)
          .label("")
          .ratio(zero_div(ram_usage_gb, ram_total_gb));
        f.render_widget(w, r);
      }
    }
  }

  fn render(&mut self, f: &mut Frame) {
    let label_l = format!(
      "{} ({}{}+{}{}+{}GPU {}GB)",
      self.soc.chip_name,
      self.soc.ecpu_cores,
      self.soc.ecpu_label,
      self.soc.pcpu_cores,
      self.soc.pcpu_label,
      self.soc.gpu_cores,
      self.soc.memory_gb,
    );

    let rows = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Fill(2), Constraint::Fill(1)].as_ref())
      .split(f.area());

    let brand = format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let block = self.title_block(&label_l, &brand);
    let iarea = block.inner(rows[0]);
    f.render_widget(block, rows[0]);

    let iarea = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Fill(1), Constraint::Fill(1)].as_ref())
      .split(iarea);

    // 1st row
    let (c1, c2) = h_stack(iarea[0]);
    let ecpu_block_label = format!("{}-CPU", self.soc.ecpu_label);
    let pcpu_block_label = format!("{}-CPU", self.soc.pcpu_label);
    self.render_freq_block(f, c1, &ecpu_block_label, &self.ecpu_freq);
    self.render_freq_block(f, c2, &pcpu_block_label, &self.pcpu_freq);

    // 2nd row
    let (c1, c2) = h_stack(iarea[1]);
    self.render_mem_block(f, c1, &self.mem);
    self.render_freq_block(f, c2, "GPU", &self.igpu_freq);

    // 3rd row
    let label_l = format!(
      "Power: {:.2}W (avg {:.2}W, max {:.2}W)",
      self.all_power.top_value, self.all_power.avg_value, self.all_power.max_value,
    );

    // Show label only if sensor is available
    let label_r = if self.sys_power.top_value > 0.0 {
      format!(
        "Total {:.2}W ({:.2}, {:.2})",
        self.sys_power.top_value, self.sys_power.avg_value, self.sys_power.max_value
      )
    } else {
      "".to_string()
    };

    let block = self.title_block(&label_l, &label_r);
    let usage = format!(" 'q' – quit, 'c' – color, 'v' – view | -/+ {}ms ", self.cfg.interval);
    let block = block.title_bottom(Line::from(usage).right_aligned());
    let iarea = block.inner(rows[1]);
    f.render_widget(block, rows[1]);

    let ha = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)].as_ref())
      .split(iarea);

    f.render_widget(self.get_power_block("CPU", &self.cpu_power, self.cpu_temp.last()), ha[0]);
    f.render_widget(self.get_power_block("GPU", &self.gpu_power, self.gpu_temp.last()), ha[1]);
    f.render_widget(self.get_power_block("ANE", &self.ane_power, 0.0), ha[2]);
  }

  pub fn run_loop(&mut self, interval: Option<u32>) -> WithError<()> {
    // use from arg if provided, otherwise use config restored value
    self.cfg.interval = interval.unwrap_or(self.cfg.interval).clamp(100, 10_000);
    let msec = Arc::new(RwLock::new(self.cfg.interval));

    let (tx, rx) = mpsc::channel::<Event>();
    run_inputs_thread(tx.clone(), 250);
    run_sampler_thread(tx.clone(), msec.clone());

    let mut term = enter_term();

    loop {
      term.draw(|f| self.render(f)).unwrap();

      match rx.recv()? {
        Event::Quit => {
          if let Some(ref path) = self.log_path {
            let fallback = chrono::Local::now();
            let start = self.start_time.as_ref().unwrap_or(&fallback);
            let footer = log_session_footer(&self.soc, path, self.log_counter, &self.peaks, start);
            if let Ok(mut file) = OpenOptions::new().append(true).open(path) {
              let _ = file.write_all(footer.as_bytes());
              let _ = file.flush();
            }
          }
          break;
        }
        Event::Update(data) => self.update_metrics(data),
        Event::ChangeColor => self.cfg.next_color(),
        Event::ChangeView => self.cfg.next_view_type(),
        Event::IncInterval => {
          self.cfg.inc_interval();
          *msec.write().unwrap() = self.cfg.interval;
        }
        Event::DecInterval => {
          self.cfg.dec_interval();
          *msec.write().unwrap() = self.cfg.interval;
        }
        _ => {}
      }
    }

    leave_term();
    Ok(())
  }
}
