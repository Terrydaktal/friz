use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
use rayon::prelude::*;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{self, Color},
    terminal::{self, ClearType, DisableLineWrap, EnableLineWrap},
};
use frizbee::MatchIndices;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

const CHUNK_SIZE: usize = 20_000;

// 1. CLI ARGUMENT PARSER
#[derive(Clone)]
struct AppConfig {
    height_percent: Option<f32>,
    reverse: bool,
    header: Option<String>,
}

fn parse_args() -> AppConfig {
    let mut config = AppConfig { height_percent: None, reverse: false, header: None };
    let mut args = std::env::args().skip(1);
    
    while let Some(arg) = args.next() {
        if arg == "--reverse" {
            config.reverse = true;
        } else if arg.starts_with("--height=") {
            if let Some(val) = arg.strip_prefix("--height=") {
                if let Ok(pct) = val.trim_end_matches('%').parse::<f32>() {
                    config.height_percent = Some(pct / 100.0);
                }
            }
        } else if arg == "--height" {
            if let Some(val) = args.next() {
                if let Ok(pct) = val.trim_end_matches('%').parse::<f32>() {
                    config.height_percent = Some(pct / 100.0);
                }
            }
        } else if arg.starts_with("--header=") {
            config.header = Some(arg.strip_prefix("--header=").unwrap().to_string());
        }
    }
    config
}

struct State {
    chunks: Mutex<Vec<Arc<Vec<String>>>>,
    total_len: AtomicUsize,
    query: Mutex<String>,
    matches: Mutex<Arc<Vec<MatchIndices>>>, 
    selection_index: AtomicUsize,
    ui_version: AtomicUsize,
    query_version: AtomicUsize,
}

fn main() -> io::Result<()> {
    let app_config = parse_args();

    let tty_in = File::open("/dev/tty")?;
    let tty_out_file = File::options().write(true).open("/dev/tty")?;
    let piped_stdin_fd = unsafe { libc::dup(io::stdin().as_raw_fd()) };
    let piped_file = unsafe { File::from_raw_fd(piped_stdin_fd) };
    unsafe { libc::dup2(tty_in.as_raw_fd(), io::stdin().as_raw_fd()); }

    let mut tty_out = BufWriter::with_capacity(8192, tty_out_file);

    let state = Arc::new(State {
        chunks: Mutex::new(Vec::new()),
        total_len: AtomicUsize::new(0),
        query: Mutex::new(String::new()),
        matches: Mutex::new(Arc::new(Vec::new())),
        selection_index: AtomicUsize::new(0),
        ui_version: AtomicUsize::new(0),
        query_version: AtomicUsize::new(0),
    });

    // 2. BACKGROUND READER (Hyper-Optimized Allocations)
    let state_reader = Arc::clone(&state);
    thread::spawn(move || {
        let mut reader = BufReader::with_capacity(1024 * 1024, piped_file);
        let mut local_batch = Vec::with_capacity(CHUNK_SIZE);
        let mut line = String::new();

        while let Ok(bytes_read) = reader.read_line(&mut line) {
            if bytes_read == 0 { break; } 
            
            if line.ends_with('\n') { line.pop(); }
            if line.ends_with('\r') { line.pop(); }
            
            local_batch.push(line.clone());
            line.clear();
            
            if local_batch.len() >= CHUNK_SIZE {
                let chunk = Arc::new(local_batch);
                local_batch = Vec::with_capacity(CHUNK_SIZE);
                
                {
                    let mut chunks = state_reader.chunks.lock().unwrap();
                    chunks.push(chunk);
                }
                state_reader.total_len.fetch_add(CHUNK_SIZE, Ordering::SeqCst);
                
                let q = state_reader.query.lock().unwrap();
                if !q.is_empty() { 
                    state_reader.query_version.fetch_add(1, Ordering::SeqCst); 
                }
                state_reader.ui_version.fetch_add(1, Ordering::SeqCst);
            }
        }
        
        if !local_batch.is_empty() {
            let chunk_len = local_batch.len();
            let chunk = Arc::new(local_batch);
            {
                let mut chunks = state_reader.chunks.lock().unwrap();
                chunks.push(chunk);
            }
            state_reader.total_len.fetch_add(chunk_len, Ordering::SeqCst);
            let q = state_reader.query.lock().unwrap();
            if !q.is_empty() { 
                state_reader.query_version.fetch_add(1, Ordering::SeqCst); 
            }
            state_reader.ui_version.fetch_add(1, Ordering::SeqCst);
        }
    });

    // 3. BACKGROUND SEARCH (Atomic-Fast Aborts)
    let state_search = Arc::clone(&state);
    thread::spawn(move || {
        let mut last_query_version = 0;
        let mut last_total_len = 0;

        loop {
            thread::sleep(Duration::from_millis(5));
            
            let current_q_ver = state_search.query_version.load(Ordering::SeqCst);
            let current_total_len = state_search.total_len.load(Ordering::SeqCst);

            let q_changed = current_q_ver != last_query_version;
            let h_changed = current_total_len != last_total_len && !state_search.query.lock().unwrap().is_empty();

            if q_changed || h_changed {
                last_query_version = current_q_ver;
                last_total_len = current_total_len;

                let (query, chunks) = {
                    let q = state_search.query.lock().unwrap().clone();
                    let c = state_search.chunks.lock().unwrap().clone();
                    (q, c)
                };

                if query.is_empty() {
                    if state_search.query_version.load(Ordering::SeqCst) == current_q_ver {
                        *state_search.matches.lock().unwrap() = Arc::new(Vec::new());
                        state_search.selection_index.store(0, Ordering::SeqCst);
                        state_search.ui_version.fetch_add(1, Ordering::SeqCst);
                    }
                    continue;
                }

                let terms: Vec<String> = query.split_whitespace().map(|s| s.to_lowercase()).collect();
                let state_search_par = Arc::clone(&state_search);

                let mut new_matches: Vec<MatchIndices> = chunks.par_iter().enumerate().filter_map(|(chunk_idx, chunk)| {
                    // ATOMIC ABORT: No more Mutex locking for every chunk!
                    if state_search_par.query_version.load(Ordering::Relaxed) != current_q_ver {
                        return None; 
                    }

                    let mut processed = Vec::new();

                    for (item_idx, line) in chunk.iter().enumerate() {
                        let line_lower = line.to_lowercase();
                        let mut all_found = true;
                        let mut indices = Vec::new();

                        for part in &terms {
                            let mut search_start = 0;
                            let mut found_part = false;
                            
                            while let Some(pos) = line_lower[search_start..].find(part) {
                                let actual_pos = search_start + pos;
                                if line_lower.len() == line.len() {
                                    for i in 0..part.len() { indices.push(actual_pos + i); }
                                } else {
                                    indices.push(actual_pos);
                                }
                                found_part = true;
                                search_start = actual_pos + part.len();
                                if part.is_empty() { break; }
                            }

                            if !found_part {
                                all_found = false;
                                break;
                            }
                        }

                        if all_found {
                            indices.sort_unstable();
                            indices.dedup();

                            // Prioritize paths whose final segment (file/final dir name)
                            // contains one or more query terms.
                            let trimmed = line_lower.trim_end_matches('/');
                            let tail = if trimmed.is_empty() {
                                line_lower.as_str()
                            } else {
                                trimmed.rsplit('/').next().unwrap_or(trimmed)
                            };
                            let tail_term_hits = terms
                                .iter()
                                .filter(|part| !part.is_empty() && tail.contains(part.as_str()))
                                .count() as i32;
                            let tail_bonus = if tail_term_hits > 0 {
                                4000 + (tail_term_hits - 1) * 1500
                            } else {
                                0
                            };
                            let base_score = 20000i32.saturating_sub(line.len() as i32);
                            let final_score =
                                (base_score + tail_bonus).clamp(0, u16::MAX as i32) as u16;

                            processed.push(MatchIndices {
                                index: (chunk_idx * CHUNK_SIZE + item_idx) as u32,
                                score: final_score,
                                indices,
                                exact: true,
                            });
                        }
                    }
                    Some(processed)
                }).flatten().collect();

                if state_search.query_version.load(Ordering::SeqCst) == current_q_ver {
                    new_matches.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
                    *state_search.matches.lock().unwrap() = Arc::new(new_matches);
                    state_search.selection_index.store(0, Ordering::SeqCst);
                    state_search.ui_version.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    });

    // 4. TUI SETUP WITH ANCHOR FIX
    terminal::enable_raw_mode()?;
    execute!(tty_out, cursor::Hide, DisableLineWrap)?;

    let mut last_rendered_height = 0;
    let mut last_rendered_version = 0;

    // 5. EVENT LOOP
    loop {
        let version = {
            let total_results = {
                let q = state.query.lock().unwrap();
                if q.is_empty() { state.total_len.load(Ordering::SeqCst) } else { state.matches.lock().unwrap().len() }
            };
            let mut sel = state.selection_index.load(Ordering::SeqCst);
            if total_results > 0 && sel >= total_results {
                sel = total_results.saturating_sub(1);
                state.selection_index.store(sel, Ordering::SeqCst);
                state.ui_version.fetch_add(1, Ordering::SeqCst);
            }
            state.ui_version.load(Ordering::SeqCst)
        };

        if version != last_rendered_version {
            let (q, sel, matches, chunks, total_len) = {
                let q_val = state.query.lock().unwrap().clone();
                let matches_val = Arc::clone(&*state.matches.lock().unwrap());
                let chunks_val = state.chunks.lock().unwrap().clone();
                (
                    q_val,
                    state.selection_index.load(Ordering::SeqCst),
                    matches_val,
                    chunks_val,
                    state.total_len.load(Ordering::SeqCst),
                )
            };
            last_rendered_height = render(
                &mut tty_out, 
                &q, 
                sel, 
                &matches, 
                &chunks, 
                total_len,
                last_rendered_height,
                &app_config
            )?;
            last_rendered_version = version;
        }

        if event::poll(Duration::from_millis(5))? {
            loop {
                if let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? {
                    match (code, modifiers) {
                        (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            cleanup(&mut tty_out, last_rendered_height)?;
                            return Ok(());
                        }
                        (KeyCode::Enter, _) => {
                            let (q, matches, chunks) = {
                                (state.query.lock().unwrap().clone(), Arc::clone(&*state.matches.lock().unwrap()), state.chunks.lock().unwrap().clone())
                            };
                            let total_results = if q.is_empty() { state.total_len.load(Ordering::SeqCst) } else { matches.len() };
                            let sel = state.selection_index.load(Ordering::SeqCst);

                            if total_results > 0 && sel < total_results {
                                let global_idx = if q.is_empty() { sel } else { matches[sel].index as usize };
                                let chunk_idx = global_idx / CHUNK_SIZE;
                                let item_idx = global_idx % CHUNK_SIZE;
                                let selected = chunks[chunk_idx][item_idx].clone();
                                
                                cleanup(&mut tty_out, last_rendered_height)?;
                                println!("{}", selected);
                                return Ok(());
                            }
                        }
                        (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            let total_results = {
                                let q = state.query.lock().unwrap();
                                if q.is_empty() { state.total_len.load(Ordering::SeqCst) } else { state.matches.lock().unwrap().len() }
                            };
                            let sel = state.selection_index.load(Ordering::SeqCst);
                            if app_config.reverse {
                                if sel > 0 { state.selection_index.store(sel - 1, Ordering::SeqCst); state.ui_version.fetch_add(1, Ordering::SeqCst); }
                            } else {
                                if sel + 1 < total_results { state.selection_index.store(sel + 1, Ordering::SeqCst); state.ui_version.fetch_add(1, Ordering::SeqCst); }
                            }
                        }
                        (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            let total_results = {
                                let q = state.query.lock().unwrap();
                                if q.is_empty() { state.total_len.load(Ordering::SeqCst) } else { state.matches.lock().unwrap().len() }
                            };
                            let sel = state.selection_index.load(Ordering::SeqCst);
                            if app_config.reverse {
                                if sel + 1 < total_results { state.selection_index.store(sel + 1, Ordering::SeqCst); state.ui_version.fetch_add(1, Ordering::SeqCst); }
                            } else {
                                if sel > 0 { state.selection_index.store(sel - 1, Ordering::SeqCst); state.ui_version.fetch_add(1, Ordering::SeqCst); }
                            }
                        }
                        (KeyCode::Backspace, KeyModifiers::CONTROL) | (KeyCode::Char('w'), KeyModifiers::CONTROL) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                            let mut q = state.query.lock().unwrap();
                            let mut new_query = q.trim_end_matches(|c: char| c == '/' || c == ' ').to_string();
                            if let Some(last_pos) = new_query.rfind(|c: char| c == '/' || c == ' ') {
                                new_query.truncate(last_pos + 1);
                                *q = new_query;
                            } else {
                                q.clear();
                            }
                            state.query_version.fetch_add(1, Ordering::SeqCst);
                            state.ui_version.fetch_add(1, Ordering::SeqCst);
                        }
                        (KeyCode::Backspace, _) => {
                            let mut q = state.query.lock().unwrap();
                            q.pop();
                            state.query_version.fetch_add(1, Ordering::SeqCst);
                            state.ui_version.fetch_add(1, Ordering::SeqCst);
                        }
                        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
                            let mut q = state.query.lock().unwrap();
                            q.push(c);
                            state.query_version.fetch_add(1, Ordering::SeqCst);
                            state.ui_version.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                if !event::poll(Duration::from_millis(0))? { break; }
            }
        } 
    }
}

// 6. BI-DIRECTIONAL LAYOUT RENDERER
fn render<W: Write>(
    w: &mut W,
    query: &str,
    selection_index: usize,
    matches: &[MatchIndices],
    chunks: &[Arc<Vec<String>>],
    total_len: usize,
    last_height: usize,
    config: &AppConfig,
) -> io::Result<usize> {
    
    let (_, term_height) = terminal::size()?;
    let max_display = if let Some(pct) = config.height_percent {
        (term_height as f32 * pct).round() as usize
    } else {
        (term_height as usize).saturating_sub(2).min(15)
    };

    let total_results = if query.is_empty() { total_len } else { matches.len() };
    let display_count = total_results.min(max_display);
    let query_terms: Vec<String> = query
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect();
    
    let header_lines = if config.header.is_some() { 1 } else { 0 };
    let vertical_span = display_count + 2 + header_lines; // +2 for Prompt and Info Line

    // Clear previous frame
    queue!(w, cursor::MoveToColumn(0))?;
    if last_height > 0 {
        queue!(w, cursor::MoveUp(last_height as u16))?;
    }
    queue!(w, terminal::Clear(ClearType::FromCursorDown))?;

    let draw_match = |w: &mut W, i: usize| -> io::Result<()> {
        let (global_idx, char_indices) = if query.is_empty() {
            (i, &[] as &[usize])
        } else {
            let m = &matches[i];
            (m.index as usize, m.indices.as_slice())
        };
        
        let chunk_idx = global_idx / CHUNK_SIZE;
        let item_idx = global_idx % CHUNK_SIZE;
        let line = &chunks[chunk_idx][item_idx];
        let is_selected = i == selection_index;
        let has_basename_term = if query_terms.is_empty() {
            true
        } else {
            let line_lower = line.to_lowercase();
            let trimmed = line_lower.trim_end_matches('/');
            let tail = if trimmed.is_empty() {
                line_lower.as_str()
            } else {
                trimmed.rsplit('/').next().unwrap_or(trimmed)
            };
            query_terms
                .iter()
                .any(|term| tail.contains(term.as_str()))
        };
        let base_color = if is_selected {
            Color::Rgb { r: 255, g: 255, b: 255 } // #ffffff
        } else if !has_basename_term {
            Color::Rgb { r: 107, g: 107, b: 107 } // #6b6b6b
        } else {
            Color::Rgb { r: 209, g: 209, b: 209 } // #d1d1d1
        };
        
        queue!(w, style::SetForegroundColor(base_color))?;
        if is_selected {
            queue!(w, style::SetAttribute(style::Attribute::Bold))?;
            write!(w, "> ")?;
        } else {
            write!(w, "  ")?;
        }

        for (char_idx, c) in line.char_indices() {
            if char_indices.contains(&char_idx) {
                queue!(w, style::SetForegroundColor(Color::Red))?;
                write!(w, "{}", c)?;
                queue!(w, style::SetForegroundColor(base_color))?;
            } else {
                write!(w, "{}", c)?;
            }
        }
        write!(w, "\r\n")?;
        queue!(w, style::SetAttribute(style::Attribute::Reset))?;
        queue!(w, style::ResetColor)?;
        Ok(())
    };

    let count_info = format!("{}/{}", total_results, total_len);
    let info_width: usize = 30; // Fixed width for the count + dashes
    let dash_count = info_width.saturating_sub(count_info.len());
    let dashes: String = "─".repeat(dash_count);

    if config.reverse {
        write!(w, "> {}\r\n", query)?;
        
        // Render Info Line (White count, Grey dashes)
        write!(w, "  ")?;
        queue!(w, style::SetForegroundColor(Color::White))?;
        write!(w, "{}", count_info)?;
        queue!(w, style::SetForegroundColor(Color::DarkGrey))?;
        write!(w, " {}\r\n", dashes)?;
        queue!(w, style::ResetColor)?;
        
        if let Some(header) = &config.header {
            queue!(w, style::SetForegroundColor(Color::Cyan))?;
            write!(w, "  {}\r\n", header)?;
            queue!(w, style::ResetColor)?;
        }

        let start = if selection_index >= display_count { selection_index - display_count + 1 } else { 0 };
        let end = (start + display_count).min(total_results);
        for i in start..end { draw_match(w, i)?; }
        
    } else {
        let start = if selection_index >= display_count { selection_index - display_count + 1 } else { 0 };
        let end = (start + display_count).min(total_results);
        
        for i in (start..end).rev() { draw_match(w, i)?; }

        if let Some(header) = &config.header {
            queue!(w, style::SetForegroundColor(Color::Cyan))?;
            write!(w, "  {}\r\n", header)?;
            queue!(w, style::ResetColor)?;
        }

        // Render Info Line (White count, Grey dashes)
        write!(w, "  ")?;
        queue!(w, style::SetForegroundColor(Color::White))?;
        write!(w, "{}", count_info)?;
        queue!(w, style::SetForegroundColor(Color::DarkGrey))?;
        write!(w, " {}\r\n", dashes)?;
        queue!(w, style::ResetColor)?;
        
        write!(w, "> {}", query)?;
    }

    w.flush()?;
    
    if config.reverse {
        Ok(vertical_span)
    } else {
        Ok(vertical_span - 1)
    }
}

fn cleanup<W: Write>(w: &mut W, last_height: usize) -> io::Result<()> {
    queue!(w, cursor::MoveToColumn(0))?;
    if last_height > 0 {
        queue!(w, cursor::MoveUp(last_height as u16))?;
    }
    queue!(w, terminal::Clear(ClearType::FromCursorDown))?;
    queue!(w, cursor::Show, EnableLineWrap)?;
    w.flush()?;
    terminal::disable_raw_mode()?;
    Ok(())
}
