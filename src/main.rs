use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{self, Color},
    terminal::{self, ClearType, DisableLineWrap, EnableLineWrap},
};
use frizbee::{match_list_indices, Config, MatchIndices};
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
    chunks: Vec<Arc<Vec<String>>>,
    total_len: usize,
    query: String,
    matches: Vec<MatchIndices>, 
    selection_index: usize,
    ui_version: usize,
    query_version: usize,
}

fn main() -> io::Result<()> {
    let app_config = parse_args();

    let tty_in = File::open("/dev/tty")?;
    let tty_out_file = File::options().write(true).open("/dev/tty")?;
    let piped_stdin_fd = unsafe { libc::dup(io::stdin().as_raw_fd()) };
    let piped_file = unsafe { File::from_raw_fd(piped_stdin_fd) };
    unsafe { libc::dup2(tty_in.as_raw_fd(), io::stdin().as_raw_fd()); }

    let mut tty_out = BufWriter::with_capacity(8192, tty_out_file);

    let state = Arc::new(Mutex::new(State {
        chunks: Vec::new(),
        total_len: 0,
        query: String::new(),
        matches: Vec::new(),
        selection_index: 0,
        ui_version: 0,
        query_version: 0,
    }));

    // 2. BACKGROUND READER (No False Aborts!)
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
                
                let mut s = state_reader.lock().unwrap();
                s.chunks.push(chunk);
                s.total_len += CHUNK_SIZE;
                // WE DELETED query_version += 1 HERE! 
                s.ui_version += 1;
            }
        }
        
        if !local_batch.is_empty() {
            let chunk_len = local_batch.len();
            let chunk = Arc::new(local_batch);
            let mut s = state_reader.lock().unwrap();
            s.chunks.push(chunk);
            s.total_len += chunk_len;
            s.ui_version += 1;
        }
    });

    // 3. BACKGROUND SEARCH (Bulletproof Full-Sweep)
    let state_search = Arc::clone(&state);
    thread::spawn(move || {
        let mut last_query_version = 0;
        let mut last_total_len = 0;
        let mut last_completed_query = String::new();

        loop {
            thread::sleep(Duration::from_millis(5));
            
            let (needs_search, query, chunks, total_len, current_q_ver) = {
                let s = state_search.lock().unwrap();
                let q_changed = s.query_version != last_query_version;
                let h_changed = s.total_len != last_total_len && !s.query.is_empty();

                if q_changed || h_changed {
                    (true, s.query.clone(), s.chunks.clone(), s.total_len, s.query_version) 
                } else {
                    (false, String::new(), Vec::new(), 0, 0)
                }
            };
            
            if needs_search {
                last_query_version = current_q_ver;
                last_total_len = total_len;

                // THE OPTIMIZATION: Reusing this buffer prevents 3.2 million 
                // pointer allocations per keystroke, making full sweeps lightning fast!
                let mut refs_buffer: Vec<&str> = Vec::with_capacity(CHUNK_SIZE);

                // Handle empty query instantly
                if query.is_empty() {
                    let mut s = state_search.lock().unwrap();
                    if s.query_version == current_q_ver {
                        s.matches = Vec::new();
                        s.selection_index = 0;
                        s.ui_version += 1;
                    }
                    continue;
                }

                let mut new_matches = Vec::new();
                let mut aborted = false;

                let mut check_abort = || {
                    if state_search.lock().unwrap().query_version != current_q_ver {
                        aborted = true;
                        true
                    } else {
                        false
                    }
                };

                let mut last_flush = Instant::now(); 

                // ALWAYS do a full sweep, safely ignoring narrowed memory!
                for (chunk_idx, chunk) in chunks.iter().enumerate() {
                    if check_abort() { break; } 

                    // Clear and fill the reusable buffer
                    refs_buffer.clear();
                    for s in chunk.iter() {
                        refs_buffer.push(s.as_str());
                    }

                    let local_matches = match_list_indices(&query, &refs_buffer, &Config::default());

                    for mut m in local_matches {
                        m.index += (chunk_idx * CHUNK_SIZE) as u32;
                        new_matches.push(m);
                    }

                    // Progressive Rendering (Keeps the UI feeling instant)
                    if last_flush.elapsed() > Duration::from_millis(16) {
                        let mut temp_matches = new_matches.clone();
                        temp_matches.sort_unstable_by(|a, b| {
                            b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index))
                        });

                        let mut s = state_search.lock().unwrap();
                        if s.query_version == current_q_ver {
                            s.matches = temp_matches;
                            s.ui_version += 1;
                        }
                        last_flush = Instant::now();
                    }
                }

                if !aborted {
                    new_matches.sort_unstable_by(|a, b| {
                        b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index))
                    });

                    let mut s = state_search.lock().unwrap();
                    if s.query_version == current_q_ver {
                        s.matches = new_matches;
                        // Only reset cursor if the user actually changed the text
                        if query != last_completed_query {
                            s.selection_index = 0;
                        }
                        s.ui_version += 1;
                        last_completed_query = query.clone();
                    }
                }
            }
        }
    });

    // 4. TUI SETUP
    terminal::enable_raw_mode()?;
    execute!(tty_out, cursor::Hide, DisableLineWrap)?;

    let (_, term_height) = terminal::size()?;
    let max_display = if let Some(pct) = app_config.height_percent {
        (term_height as f32 * pct).round() as usize
    } else {
        (term_height as usize).saturating_sub(2).min(15)
    };
    
    let header_lines = if app_config.header.is_some() { 1 } else { 0 };
    let reserved_lines = max_display + 2 + header_lines;
    for _ in 0..reserved_lines { write!(tty_out, "\r\n")?; }
    queue!(tty_out, cursor::MoveUp(reserved_lines as u16))?;
    tty_out.flush()?;

    let mut last_rendered_height = 0;
    let mut last_rendered_version = 0;

    // 5. EVENT LOOP
    loop {
        let version = {
            let mut s = state.lock().unwrap();
            let total_results = if s.query.is_empty() { s.total_len } else { s.matches.len() };
            if total_results > 0 && s.selection_index >= total_results {
                s.selection_index = total_results.saturating_sub(1);
                s.ui_version += 1;
            }
            s.ui_version
        };

        let render_data = if version != last_rendered_version {
            let s = state.lock().unwrap();
            Some((
                s.query.clone(),
                s.selection_index,
                s.matches.clone(),
                s.chunks.clone(),
                s.total_len,
            ))
        } else {
            None
        };

        if let Some((q, sel, matches, chunks, total_len)) = render_data {
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
                    let mut s = state.lock().unwrap();
                    let total_results = if s.query.is_empty() { s.total_len } else { s.matches.len() };

                    match (code, modifiers) {
                        (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            cleanup(&mut tty_out, last_rendered_height)?;
                            return Ok(());
                        }
                        (KeyCode::Enter, _) => {
                            if total_results > 0 && s.selection_index < total_results {
                                let global_idx = if s.query.is_empty() {
                                    s.selection_index
                                } else {
                                    s.matches[s.selection_index].index as usize
                                };
                                
                                let chunk_idx = global_idx / CHUNK_SIZE;
                                let item_idx = global_idx % CHUNK_SIZE;
                                let selected = s.chunks[chunk_idx][item_idx].clone();
                                
                                cleanup(&mut tty_out, last_rendered_height)?;
                                println!("{}", selected);
                                return Ok(());
                            }
                        }
                        (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            if app_config.reverse {
                                if s.selection_index > 0 { s.selection_index -= 1; s.ui_version += 1; }
                            } else {
                                if s.selection_index + 1 < total_results { s.selection_index += 1; s.ui_version += 1; }
                            }
                        }
                        (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            if app_config.reverse {
                                if s.selection_index + 1 < total_results { s.selection_index += 1; s.ui_version += 1; }
                            } else {
                                if s.selection_index > 0 { s.selection_index -= 1; s.ui_version += 1; }
                            }
                        }
                        (KeyCode::Backspace, KeyModifiers::CONTROL) | (KeyCode::Char('w'), KeyModifiers::CONTROL) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                            let mut new_query = s.query.trim_end_matches(|c: char| c == '/' || c == ' ').to_string();
                            if let Some(last_pos) = new_query.rfind(|c: char| c == '/' || c == ' ') {
                                new_query.truncate(last_pos + 1);
                                s.query = new_query;
                            } else {
                                s.query.clear();
                            }
                            s.query_version += 1;
                            s.ui_version += 1;
                        }
                        (KeyCode::Backspace, _) => {
                            s.query.pop();
                            s.query_version += 1;
                            s.ui_version += 1;
                        }
                        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
                            s.query.push(c);
                            s.query_version += 1;
                            s.ui_version += 1;
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
    
    let header_lines = if config.header.is_some() { 1 } else { 0 };
    let vertical_span = display_count + 1 + header_lines;

    queue!(w, cursor::MoveToColumn(0))?;
    if !config.reverse && last_height > 0 {
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
        
        if i == selection_index {
            queue!(w, style::SetForegroundColor(Color::Red))?; 
            write!(w, "> ")?;
        } else {
            write!(w, "  ")?;
        }

        for (char_idx, c) in line.char_indices() {
            if char_indices.contains(&char_idx) {
                queue!(w, style::SetForegroundColor(Color::Red))?;
                write!(w, "{}", c)?;
                if i == selection_index {
                    queue!(w, style::SetForegroundColor(Color::Red))?;
                } else {
                    queue!(w, style::ResetColor)?;
                }
            } else {
                write!(w, "{}", c)?;
            }
        }
        write!(w, "\r\n")?;
        queue!(w, style::ResetColor)?;
        Ok(())
    };

    if config.reverse {
        write!(w, "> {}\r\n", query)?;
        queue!(w, style::SetForegroundColor(Color::DarkGrey))?;
        write!(w, "  {}/{} ────────────────────\r\n", total_results, total_len)?;
        queue!(w, style::ResetColor)?;
        
        if let Some(header) = &config.header {
            queue!(w, style::SetForegroundColor(Color::Cyan))?;
            write!(w, "  {}\r\n", header)?;
            queue!(w, style::ResetColor)?;
        }

        let start = if selection_index >= display_count { selection_index - display_count + 1 } else { 0 };
        let end = (start + display_count).min(total_results);
        for i in start..end { draw_match(w, i)?; }

        queue!(
            w,
            cursor::MoveUp((vertical_span + 1) as u16),
            cursor::MoveToColumn((query.len() + 2) as u16)
        )?;
        
    } else {
        let start = if selection_index >= display_count { selection_index - display_count + 1 } else { 0 };
        let end = (start + display_count).min(total_results);
        
        for i in (start..end).rev() { draw_match(w, i)?; }

        if let Some(header) = &config.header {
            queue!(w, style::SetForegroundColor(Color::Cyan))?;
            write!(w, "  {}\r\n", header)?;
            queue!(w, style::ResetColor)?;
        }

        queue!(w, style::SetForegroundColor(Color::DarkGrey))?;
        write!(w, "  {}/{} ────────────────────\r\n", total_results, total_len)?;
        queue!(w, style::ResetColor)?;
        
        write!(w, "> {}", query)?;
    }

    w.flush()?;
    Ok(vertical_span)
}

fn cleanup<W: Write>(w: &mut W, last_height: usize) -> io::Result<()> {
    queue!(w, cursor::MoveToColumn(0))?;
    if last_height > 0 {
        queue!(w, cursor::MoveUp(last_height as u16))?;
    }
    queue!(w, terminal::Clear(ClearType::FromCursorDown), cursor::Show, EnableLineWrap)?;
    w.flush()?;
    terminal::disable_raw_mode()?;
    Ok(())
}