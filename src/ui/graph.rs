//! The flow as a graph: the chief over a row of agent cards, joined by the
//! lines their messages travel.
//!
//! The first version was a list with bars — who talked to whom, as text. It
//! answered the question and looked nothing like the design, which drew the
//! fleet as what it is: one card to brief, fanning out to the cards that do
//! the work. zoetrope draws its graphs the same way, and adds the part that
//! makes it read as live: a line lights up while the agent at its end is
//! working.
//!
//! Drawn here rather than through a graph crate because the shape never
//! changes — one hub, one row, the odd line straight between two agents — and
//! a layout engine would be solving a problem this does not have. Everything
//! goes onto a canvas as wide as the row needs; when that is wider than the
//! pane, the view pans to keep the selected card in sight.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::db::Event;
use crate::ui::crew::{Presence, Role, Row};
use crate::ui::theme;

/// How long a message is shown in flight along its line.
pub const PULSE_SECS: f64 = 1.6;
/// How long a line stays lit after a message went along it.
const GLOW_SECS: f64 = 12.0;

const CHIEF_H: u16 = 4;
const CARD_H: u16 = 5;
const GAP: u16 = 2;
const MIN_CARD_W: u16 = 18;
const MAX_CARD_W: u16 = 26;

// Rows, from the top of the canvas. The chief's card, then the trunk down
// from it, the bus it fans out along, one row of drop lines into the cards,
// the cards, and whatever runs between cards underneath them.
const TRUNK_Y: u16 = CHIEF_H;
const BUS_Y: u16 = TRUNK_Y + 1;
const DROP_Y: u16 = BUS_Y + 1;
const CARDS_Y: u16 = DROP_Y + 1;
#[cfg(test)]
const UNDER_Y: u16 = CARDS_Y + CARD_H;

/// Draw the graph into `area`.
///
/// `ages` runs alongside `events`: how many seconds since each one should
/// count as having happened. Kept separate from the timestamp because a
/// message fleet only noticed two seconds after it was written should still
/// be seen travelling, not skipped.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    rows: &[Row],
    events: &[Event],
    ages: &[f64],
    selected: Option<&str>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let chief = rows.iter().find(|r| r.role == Role::Chief);
    let workers: Vec<&Row> = rows.iter().filter(|r| r.role != Role::Chief).collect();

    let traffic = Traffic::count(events, ages, chief.map(|c| c.name.as_str()), &workers);
    let grid = Layout::new(area.width, chief.is_some(), workers.len());
    let layout = grid.placed(&traffic);
    let mut canvas = Canvas::new(layout.width, layout.height);

    if let Some(chief) = chief {
        draw_chief(&mut canvas, &layout, chief, &traffic, selected);
    }
    if workers.is_empty() {
        let y = if chief.is_some() { CARDS_Y } else { 0 };
        canvas.text(
            0,
            y,
            "no agents yet — the chief starts them with fleet spawn",
            layout.width,
            theme::faint(),
        );
    } else {
        if chief.is_some() {
            draw_fan(&mut canvas, &layout, &workers, &traffic);
        }
        for (i, w) in workers.iter().enumerate() {
            draw_card(&mut canvas, &layout, i, w, selected);
        }
        draw_peers(&mut canvas, &layout, &traffic, &workers);
        draw_pulses(&mut canvas, &layout, &traffic);
        draw_legend(&mut canvas, &layout);
    }

    // Keep the selected card on screen. Cards wrap rather than run off to the
    // side, so the only way out of the pane is down, and the camera follows
    // the selection there — the way zoetrope's follows the active agent.
    let focus = selected.and_then(|name| workers.iter().position(|w| w.name == name));
    let view_y = match focus {
        Some(i) if layout.height > area.height => {
            let bottom = layout.card_y(i) + CARD_H + 1;
            bottom
                .saturating_sub(area.height)
                .min(layout.height - area.height)
        }
        _ => 0,
    };
    canvas.blit(buf, area, view_y);

    // Say that there is more, and which way.
    let right = area.x + area.width - 1;
    if view_y > 0 {
        put(buf, right, area.y, "▴", theme::accent());
    }
    if view_y + area.height < layout.height {
        put(buf, right, area.y + area.height - 1, "▾", theme::accent());
    }
}

/// Where everything goes on the canvas.
struct Layout {
    width: u16,
    height: u16,
    chief: bool,
    n: usize,
    per_row: usize,
    card_w: u16,
    /// The left edge of the grid of cards.
    row_x: u16,
    /// When the cards wrap onto several rows, the line from the chief runs
    /// down the left edge to reach each of them — an org chart's spine —
    /// rather than through the cards of the rows above.
    spine: bool,
    /// The column the spine runs down, just left of the grid.
    spine_x: u16,
    chief_x: u16,
    chief_w: u16,
    /// Per row of cards: where its bus runs, or with no chief, where its
    /// cards begin.
    row_y: Vec<u16>,
    /// Per row: how many lines between its own cards run underneath it.
    under: Vec<u16>,
    /// Direct lines between cards on different rows, written out as text:
    /// drawn, they would have to cross the cards in between.
    listed: u16,
    legend_y: u16,
}

/// Columns the spine takes at the left of a wrapped grid.
const SPINE_W: u16 = 2;

impl Layout {
    /// The grid alone: how many cards to a row, and how wide. Where the rows
    /// go vertically waits for `placed`, which knows what runs between them.
    fn new(avail: u16, chief: bool, n: usize) -> Layout {
        let n16 = n.max(1) as u16;
        let chief_w0 = 32.min(avail.max(16)).max(16);

        // One row if it fits at the smallest card; that is the design, and
        // the shape that reads best.
        let fit = (avail + GAP) / n16;
        let (per_row, card_w, spine, row_w, row_x0) = if fit >= MIN_CARD_W + GAP || n == 1 {
            let cw = fit.saturating_sub(GAP).clamp(MIN_CARD_W.min(avail), MAX_CARD_W);
            let rw = n16 * cw + (n16 - 1) * GAP;
            (n.max(1), cw, false, rw, None)
        } else {
            let lead = if chief { SPINE_W } else { 0 };
            let usable = avail.saturating_sub(lead).max(10);
            let per = ((usable + GAP) / (MIN_CARD_W + GAP)).max(1);
            let cw = ((usable + GAP) / per)
                .saturating_sub(GAP)
                .clamp(MIN_CARD_W.min(usable), MAX_CARD_W);
            let rw = per * cw + (per - 1) * GAP;
            (per as usize, cw, chief, rw, Some(lead))
        };
        let width = (row_x0.unwrap_or(0) + row_w).max(chief_w0).max(avail);
        let chief_w = chief_w0.min(width);
        let lead = row_x0.unwrap_or(0);
        let block = lead + row_w;
        let left = (width.saturating_sub(block)) / 2;
        let row_x = left + lead;
        let spine_x = left;

        let mut l = Layout {
            width,
            height: 0,
            chief,
            n,
            per_row,
            card_w,
            row_x,
            spine,
            spine_x,
            chief_x: (width - chief_w) / 2,
            chief_w,
            row_y: Vec::new(),
            under: vec![0; n.div_ceil(per_row.max(1)).max(1)],
            listed: 0,
            legend_y: 0,
        };
        l.place();
        l
    }

    /// Room under each row for the lines between its cards, and below the
    /// graph for the ones that cannot be drawn.
    fn placed(mut self, t: &Traffic) -> Layout {
        let rows = self.rows();
        self.under = vec![0; rows];
        self.listed = 0;
        for p in &t.peers {
            match self.same_row(p) {
                Some(r) => self.under[r] = (self.under[r] + 1).min(3),
                None => self.listed += 1,
            }
        }
        self.listed = self.listed.min(4);
        self.place();
        self
    }

    fn place(&mut self) {
        let rows = self.rows();
        self.row_y = Vec::with_capacity(rows);
        let mut y = if self.chief { BUS_Y } else { 0 };
        for r in 0..rows {
            self.row_y.push(y);
            let lines = if self.chief { 2 } else { 0 };
            // A blank row between blocks, which the spine crosses.
            y += lines + CARD_H + self.under[r] + 1;
        }
        let listed_y = y;
        self.legend_y = listed_y + self.listed + u16::from(self.listed > 0);
        self.height = self.legend_y + 1;
    }

    fn rows(&self) -> usize {
        self.n.div_ceil(self.per_row.max(1)).max(1)
    }

    fn row_of(&self, i: usize) -> usize {
        i / self.per_row.max(1)
    }

    fn same_row(&self, p: &Peer) -> Option<usize> {
        let r = self.row_of(p.a);
        (r == self.row_of(p.b)).then_some(r)
    }

    fn card_x(&self, i: usize) -> u16 {
        let col = (i % self.per_row.max(1)) as u16;
        self.row_x + col * (self.card_w + GAP)
    }

    fn card_centre(&self, i: usize) -> u16 {
        self.card_x(i) + self.card_w / 2
    }

    fn card_y(&self, i: usize) -> u16 {
        let r = self.row_of(i);
        self.row_y[r] + if self.chief { 2 } else { 0 }
    }

    fn bus_y(&self, r: usize) -> u16 {
        self.row_y[r]
    }

    fn drop_y(&self, r: usize) -> u16 {
        self.row_y[r] + 1
    }

    fn trunk_x(&self) -> u16 {
        self.chief_x + self.chief_w / 2
    }

    /// The cards on row `r`, as worker indices.
    fn row_cards(&self, r: usize) -> std::ops::Range<usize> {
        let start = r * self.per_row;
        start..(start + self.per_row).min(self.n)
    }
}

/// Who sent how much to whom, and which of it is recent.
#[derive(Default)]
struct Traffic {
    /// Per worker: messages from the chief, messages back to it.
    down: Vec<usize>,
    up: Vec<usize>,
    /// Per worker: seconds since the last message either way.
    recent: Vec<Option<f64>>,
    /// Between two workers, lower index first: counts each way, and recency.
    peers: Vec<Peer>,
    /// Messages in flight, and how far along their line they are.
    flying: Vec<(Flight, f64)>,
    chief_sent: usize,
}

struct Peer {
    a: usize,
    b: usize,
    ab: usize,
    ba: usize,
    recent: Option<f64>,
}

#[derive(Clone, Copy)]
enum Flight {
    Down(usize),
    Up(usize),
    Peer { pair: usize, forward: bool },
}

impl Traffic {
    fn count(events: &[Event], ages: &[f64], chief: Option<&str>, workers: &[&Row]) -> Traffic {
        let n = workers.len();
        let mut t = Traffic {
            down: vec![0; n],
            up: vec![0; n],
            recent: vec![None; n],
            ..Default::default()
        };
        let index: HashMap<&str, usize> = workers
            .iter()
            .enumerate()
            .map(|(i, w)| (w.name.as_str(), i))
            .collect();
        let mut pairs: HashMap<(usize, usize), usize> = HashMap::new();
        let fresher = |slot: &mut Option<f64>, age: f64| {
            *slot = Some(slot.map_or(age, |a: f64| a.min(age)));
        };

        for (e, &age) in events
            .iter()
            .zip(ages.iter().chain(std::iter::repeat(&f64::MAX)))
        {
            if e.kind != "message" {
                continue;
            }
            let (Some(from), Some(to)) = (e.from_agent.as_deref(), e.to_agent.as_deref()) else {
                continue;
            };
            let flying = age < PULSE_SECS;
            let is_chief = |name: &str| chief == Some(name);
            match (is_chief(from), is_chief(to), index.get(from), index.get(to)) {
                (true, false, _, Some(&w)) => {
                    t.down[w] += 1;
                    t.chief_sent += 1;
                    fresher(&mut t.recent[w], age);
                    if flying {
                        t.flying.push((Flight::Down(w), age / PULSE_SECS));
                    }
                }
                (false, true, Some(&w), _) => {
                    t.up[w] += 1;
                    fresher(&mut t.recent[w], age);
                    if flying {
                        t.flying.push((Flight::Up(w), age / PULSE_SECS));
                    }
                }
                (false, false, Some(&a), Some(&b)) if a != b => {
                    let key = (a.min(b), a.max(b));
                    let at = *pairs.entry(key).or_insert_with(|| {
                        t.peers.push(Peer {
                            a: key.0,
                            b: key.1,
                            ab: 0,
                            ba: 0,
                            recent: None,
                        });
                        t.peers.len() - 1
                    });
                    let peer = &mut t.peers[at];
                    let forward = a == peer.a;
                    if forward {
                        peer.ab += 1;
                    } else {
                        peer.ba += 1;
                    }
                    fresher(&mut peer.recent, age);
                    if flying {
                        t.flying
                            .push((Flight::Peer { pair: at, forward }, age / PULSE_SECS));
                    }
                }
                _ => {}
            }
        }
        // Nearer pairs first, so the short lines sit inside the long ones
        // instead of crossing them.
        let mut order: Vec<usize> = (0..t.peers.len()).collect();
        order.sort_by_key(|&i| t.peers[i].b - t.peers[i].a);
        let remap: HashMap<usize, usize> = order
            .iter()
            .enumerate()
            .map(|(new, &old)| (old, new))
            .collect();
        let mut taken: Vec<Option<Peer>> = t.peers.drain(..).map(Some).collect();
        t.peers = order
            .iter()
            .map(|&i| taken[i].take().expect("each once"))
            .collect();
        for (flight, _) in &mut t.flying {
            if let Flight::Peer { pair, .. } = flight {
                *pair = remap[pair];
            }
        }
        t
    }

    fn busiest(&self) -> usize {
        (0..self.down.len())
            .map(|i| self.down[i] + self.up[i])
            .max()
            .unwrap_or(0)
    }
}

fn draw_chief(canvas: &mut Canvas, l: &Layout, chief: &Row, t: &Traffic, selected: Option<&str>) {
    // The chief's name is always in the accent; its border takes the accent
    // only when selected, the same as a worker's.
    let border = if selected == Some(chief.name.as_str()) {
        theme::accent()
    } else {
        Style::default().fg(theme::FAINT)
    };
    canvas.card(l.chief_x, 0, l.chief_w, CHIEF_H, border);

    let inner = l.chief_w.saturating_sub(4);
    canvas.spread(
        l.chief_x + 2,
        1,
        inner,
        &format!("◆ {}", chief.name),
        theme::accent().add_modifier(Modifier::BOLD),
        &chief.uptime.clone().unwrap_or_default(),
        theme::faint(),
    );
    let (state, colour) = state_of(chief);
    let line = if t.chief_sent > 0 {
        format!("{state} · {} sent", t.chief_sent)
    } else {
        state.to_string()
    };
    canvas.text(l.chief_x + 2, 2, &line, inner, Style::default().fg(colour));
}

/// The trunk down from the chief, a bus along each row of cards — joined by
/// the spine when there is more than one row — and a line down into each
/// card, weighted by how much has gone along it and coloured by what the
/// agent at the end of it is doing.
fn draw_fan(canvas: &mut Canvas, l: &Layout, workers: &[&Row], t: &Traffic) {
    let tx = l.trunk_x();
    let rows = l.rows();
    canvas.put(tx, CHIEF_H - 1, "┬", theme::accent());
    canvas.put(tx, TRUNK_Y, "│", theme::dim());

    for r in 0..rows {
        let centres: Vec<u16> = l.row_cards(r).map(|i| l.card_centre(i)).collect();
        if centres.is_empty() {
            continue;
        }
        let y = l.bus_y(r);
        let last = *centres.last().expect("not empty");
        let sx = l.spine_x;
        let (lo, hi) = if r == 0 {
            let lo = if l.spine { sx.min(tx) } else { centres[0].min(tx) };
            (lo, last.max(tx))
        } else {
            (sx, last)
        };
        for x in lo..=hi {
            let spine_here = l.spine && x == sx;
            let up = (r == 0 && x == tx) || (spine_here && r > 0);
            let down = centres.contains(&x) || (spine_here && r + 1 < rows);
            canvas.put(x, y, junction(up, down, x > lo, x < hi), theme::dim());
        }
        // The spine, from this row's bus to the next one's.
        if l.spine && r + 1 < rows {
            for yy in y + 1..l.bus_y(r + 1) {
                canvas.put(sx, yy, "│", theme::dim());
            }
        }
    }

    let busiest = t.busiest();
    for (i, w) in workers.iter().enumerate() {
        let c = l.card_centre(i);
        let y = l.drop_y(l.row_of(i));
        let volume = t.down[i] + t.up[i];
        // Thickness is volume, as in the design: heavy for the busiest line,
        // dashed for one nothing has gone along yet.
        let glyph = match volume {
            0 => "╎",
            v if v == busiest => "┃",
            _ => "│",
        };
        canvas.put(c, y, glyph, Style::default().fg(presence_colour(w.presence)));

        let mut label = String::new();
        if t.down[i] > 0 {
            label.push_str(&format!("↓{}", t.down[i]));
        }
        if t.up[i] > 0 {
            if !label.is_empty() {
                label.push(' ');
            }
            label.push_str(&format!("↑{}", t.up[i]));
        }
        if !label.is_empty() {
            canvas.text(c + 2, y, &label, l.card_w / 2 - 1, theme::faint());
        }
    }

    // A line something went along recently is lit, the whole way: the path
    // is what the message took, not only its last few cells.
    for i in 0..workers.len() {
        if t.recent[i].is_some_and(|a| a < GLOW_SECS) {
            for (x, y) in path(l, t, Flight::Down(i)) {
                if y > CHIEF_H - 1 && y < l.card_y(i) {
                    canvas.restyle(x, y, theme::accent());
                }
            }
        }
    }
}

fn draw_card(canvas: &mut Canvas, l: &Layout, i: usize, w: &Row, selected: Option<&str>) {
    let x = l.card_x(i);
    let y = l.card_y(i);
    let is_selected = selected == Some(w.name.as_str());
    let border = if is_selected {
        theme::accent()
    } else {
        Style::default().fg(theme::FAINT)
    };
    canvas.card(x, y, l.card_w, CARD_H, border);
    if l.chief {
        // The line comes in through the top of the card.
        canvas.put(l.card_centre(i), y, "┴", border);
    }

    let inner = l.card_w.saturating_sub(4);
    let (glyph, _) = glyph_of(w.presence);
    let name_style = match w.presence {
        Presence::Gone | Presence::Unlinked => theme::dim(),
        _ => Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
    };
    canvas.spread(
        x + 2,
        y + 1,
        inner,
        &format!("{glyph} {}", w.name),
        name_style,
        &w.uptime.clone().unwrap_or_default(),
        theme::faint(),
    );
    let (state, colour) = state_of(w);
    let bg = if w.bg_running > 0 {
        format!("{}bg", w.bg_running)
    } else {
        String::new()
    };
    canvas.spread(
        x + 2,
        y + 2,
        inner,
        state,
        Style::default().fg(colour),
        &bg,
        Style::default().fg(theme::BUSY),
    );
    // For an agent with no session the detail is only the reason, which the
    // state line above has just said.
    let reason_only = matches!(w.detail.as_str(), "session ended" | "no session yet");
    if !reason_only {
        canvas.text(x + 2, y + 3, &w.detail, inner, theme::faint());
    }
}

/// Lines straight between two agents: the messages that went around the
/// chief rather than through it. Under the cards when both are on one row;
/// written out below the graph when they are not, since a drawn line would
/// have to cross every card in between.
fn draw_peers(canvas: &mut Canvas, l: &Layout, t: &Traffic, workers: &[&Row]) {
    let mut depth_in_row = vec![0u16; l.rows()];
    let mut listed = 0u16;
    let listed_y = l.legend_y.saturating_sub(l.listed + 1);

    for (k, p) in t.peers.iter().enumerate() {
        let lit = p.recent.is_some_and(|a| a < GLOW_SECS);
        let style = if lit { theme::accent() } else { theme::dim() };
        let label = peer_label(p);

        let Some(r) = l.same_row(p) else {
            if listed < l.listed {
                let line = format!(
                    "direct · {} {} {}",
                    workers[p.a].name,
                    label.trim(),
                    workers[p.b].name
                );
                canvas.text(0, listed_y + listed, &line, l.width, style);
                listed += 1;
            }
            continue;
        };
        let depth = depth_in_row[r];
        if depth >= l.under[r] {
            continue;
        }
        depth_in_row[r] += 1;

        let (xa, xb, y0, y) = peer_geometry(l, p, depth);
        let _ = k;
        canvas.put(xa, y0 - 1, "┬", style);
        canvas.put(xb, y0 - 1, "┬", style);
        for yy in y0..y {
            canvas.put(xa, yy, "│", style);
            canvas.put(xb, yy, "│", style);
        }
        canvas.put(xa, y, "╰", style);
        canvas.put(xb, y, "╯", style);
        for x in xa + 1..xb {
            canvas.put(x, y, "─", style);
        }
        let span = xb.saturating_sub(xa);
        let w = label.chars().count() as u16;
        if span > w + 2 {
            canvas.text(xa + (span - w) / 2, y, &label, w, style);
        }
    }
}

fn peer_label(p: &Peer) -> String {
    let mut label = String::new();
    if p.ab > 0 {
        label.push_str(&format!("{}▸", p.ab));
    }
    if p.ba > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&format!("◂{}", p.ba));
    }
    format!(" {label} ")
}

/// Where a line between two cards on one row runs: its two ends, the row
/// under the cards it starts from, and the row it turns along.
fn peer_geometry(l: &Layout, p: &Peer, depth: u16) -> (u16, u16, u16, u16) {
    let inset = (2 * depth).min(l.card_w / 2 - 2);
    let xa = l.card_x(p.a) + l.card_w - 3 - inset;
    let xb = l.card_x(p.b) + 2 + inset;
    let y0 = l.card_y(p.a) + CARD_H;
    (xa, xb, y0, y0 + depth)
}

/// The cells a message crosses, from sender to receiver.
fn path(l: &Layout, t: &Traffic, flight: Flight) -> Vec<(u16, u16)> {
    let tx = l.trunk_x();
    let chief_to = |i: usize| {
        let r = l.row_of(i);
        let c = l.card_centre(i);
        let mut cells = vec![(tx, CHIEF_H - 1), (tx, TRUNK_Y)];
        let along = |cells: &mut Vec<(u16, u16)>, from: u16, to: u16, y: u16| {
            if to >= from {
                cells.extend((from..=to).map(|x| (x, y)));
            } else {
                cells.extend((to..=from).rev().map(|x| (x, y)));
            }
        };
        if r == 0 {
            along(&mut cells, tx, c, l.bus_y(0));
        } else {
            // Left along the first bus to the spine, down it, then along this
            // row's bus to the card.
            let sx = l.spine_x;
            along(&mut cells, tx, sx, l.bus_y(0));
            cells.extend((l.bus_y(0) + 1..l.bus_y(r)).map(|y| (sx, y)));
            along(&mut cells, sx, c, l.bus_y(r));
        }
        cells.push((c, l.drop_y(r)));
        cells.push((c, l.card_y(i)));
        cells
    };
    match flight {
        Flight::Down(i) => chief_to(i),
        Flight::Up(i) => {
            let mut cells = chief_to(i);
            cells.reverse();
            cells
        }
        Flight::Peer { pair, forward } => {
            let Some(p) = t.peers.get(pair) else {
                return Vec::new();
            };
            let Some(r) = l.same_row(p) else {
                return Vec::new();
            };
            // The depth this pair was drawn at: its place among its row's.
            let depth = t.peers[..pair]
                .iter()
                .filter(|q| l.same_row(q) == Some(r))
                .count() as u16;
            if depth >= l.under[r] {
                return Vec::new();
            }
            let (xa, xb, y0, y) = peer_geometry(l, p, depth);
            let mut cells: Vec<(u16, u16)> = (y0 - 1..=y).map(|yy| (xa, yy)).collect();
            cells.extend((xa + 1..xb).map(|x| (x, y)));
            cells.extend((y0 - 1..=y).rev().map(|yy| (xb, yy)));
            if !forward {
                cells.reverse();
            }
            cells
        }
    }
}

/// A message in flight is a dot running along its line, with a short tail.
fn draw_pulses(canvas: &mut Canvas, l: &Layout, t: &Traffic) {
    for &(flight, progress) in &t.flying {
        if !l.chief && !matches!(flight, Flight::Peer { .. }) {
            continue;
        }
        let cells = path(l, t, flight);
        if cells.is_empty() {
            continue;
        }
        let at = (progress.clamp(0.0, 1.0) * (cells.len() - 1) as f64).round() as usize;
        if at > 0 {
            let (x, y) = cells[at - 1];
            canvas.put(x, y, "•", theme::accent());
        }
        let (x, y) = cells[at];
        canvas.put(x, y, "●", theme::accent().add_modifier(Modifier::BOLD));
    }
}

fn draw_legend(canvas: &mut Canvas, l: &Layout) {
    let mut parts = vec![];
    if l.chief {
        parts.push("↓ from the chief  ↑ back");
        parts.push("┃ busiest  ╎ quiet");
    }
    parts.push("● in flight");
    canvas.text(0, l.legend_y, &parts.join("   "), l.width, theme::faint());
}

fn state_of(r: &Row) -> (&'static str, Color) {
    // Short, because a card is narrow and the line under it already says why.
    if r.asking {
        return ("! needs you", theme::ACCENT);
    }
    match (r.role, r.presence) {
        (_, Presence::Working) => ("● working", theme::BUSY),
        (Role::Chief, Presence::Waiting) => ("○ waiting on you", theme::OK),
        (_, Presence::Waiting) => ("○ waiting", theme::OK),
        (_, Presence::Gone) => ("× ended", theme::FAINT),
        (_, Presence::Unlinked) => ("· not started", theme::FAINT),
    }
}

fn glyph_of(p: Presence) -> (&'static str, Color) {
    match p {
        Presence::Working => ("●", theme::BUSY),
        Presence::Waiting => ("○", theme::OK),
        Presence::Gone => ("×", theme::FAINT),
        Presence::Unlinked => ("·", theme::FAINT),
    }
}

fn presence_colour(p: Presence) -> Color {
    glyph_of(p).1
}

/// The box-drawing glyph for a cell with lines leaving it in these
/// directions.
fn junction(up: bool, down: bool, left: bool, right: bool) -> &'static str {
    match (up, down, left, right) {
        (true, true, true, true) => "┼",
        (true, true, true, false) => "┤",
        (true, true, false, true) => "├",
        (true, true, false, false) => "│",
        (true, false, true, true) => "┴",
        (false, true, true, true) => "┬",
        (false, true, false, true) => "╭",
        (false, true, true, false) => "╮",
        (true, false, false, true) => "╰",
        (true, false, true, false) => "╯",
        _ => "─",
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(s).set_style(style);
    }
}

/// A buffer the size of the whole graph, drawn into and then shown through
/// the pane as a window.
struct Canvas {
    buf: Buffer,
}

impl Canvas {
    fn new(width: u16, height: u16) -> Canvas {
        Canvas {
            buf: Buffer::empty(Rect::new(0, 0, width, height)),
        }
    }

    fn put(&mut self, x: u16, y: u16, s: &str, style: Style) {
        put(&mut self.buf, x, y, s, style);
    }

    /// Change a cell's colour and keep its glyph.
    fn restyle(&mut self, x: u16, y: u16, style: Style) {
        if let Some(cell) = self.buf.cell_mut((x, y)) {
            cell.set_style(style);
        }
    }

    /// Text cut to `width`, marked where it was cut.
    fn text(&mut self, x: u16, y: u16, s: &str, width: u16, style: Style) {
        let width = width as usize;
        if width == 0 {
            return;
        }
        let shown: String = if s.chars().count() > width {
            let cut: String = s.chars().take(width.saturating_sub(1)).collect();
            format!("{cut}…")
        } else {
            s.to_string()
        };
        self.buf.set_string(x, y, shown, style);
    }

    /// Left text and right text on one line, the right side winning.
    #[allow(clippy::too_many_arguments)]
    fn spread(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        left: &str,
        ls: Style,
        right: &str,
        rs: Style,
    ) {
        // The left side is what the line is about — a name, a state — and the
        // right is a detail about it, so the right is what gives way. Cut the
        // other way, five cards in a row read "content-… 6m".
        let lw = left.chars().count() as u16;
        let rw = right.chars().count() as u16;
        if rw > 0 && lw + 1 + rw <= width {
            self.text(x + width - rw, y, right, rw, rs);
        }
        self.text(x, y, left, width, ls);
    }

    /// A rounded card. It paints only its border; the pane's background shows
    /// through.
    fn card(&mut self, x: u16, y: u16, w: u16, h: u16, border: Style) {
        if w < 2 || h < 2 {
            return;
        }
        let right = x + w - 1;
        let bottom = y + h - 1;
        for cx in x + 1..right {
            self.put(cx, y, "─", border);
            self.put(cx, bottom, "─", border);
        }
        for cy in y + 1..bottom {
            self.put(x, cy, "│", border);
            self.put(right, cy, "│", border);
        }
        self.put(x, y, "╭", border);
        self.put(right, y, "╮", border);
        self.put(x, bottom, "╰", border);
        self.put(right, bottom, "╯", border);
    }

    /// Copy the part of the canvas that fits into `area`, from row `from`.
    fn blit(&self, buf: &mut Buffer, area: Rect, from: u16) {
        let canvas = self.buf.area;
        for dy in 0..area.height {
            let sy = from + dy;
            if sy >= canvas.height {
                break;
            }
            for dx in 0..area.width.min(canvas.width) {
                let (Some(src), Some(dst)) = (
                    self.buf.cell((dx, sy)),
                    buf.cell_mut((area.x + dx, area.y + dy)),
                ) else {
                    continue;
                };
                // Blank canvas cells leave the pane's own background alone.
                if src.symbol() == " " && src.bg == Color::Reset {
                    continue;
                }
                *dst = src.clone();
            }
        }
    }
}

/// Seconds since the epoch, from the board's own timestamp format:
/// `2026-09-20T14:11:32.123Z`, always UTC.
pub fn epoch(ts: &str) -> Option<f64> {
    let (date, time) = ts.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let (hh, mm) = (t.next()?.parse::<f64>().ok()?, t.next()?.parse::<f64>().ok()?);
    let ss = t.next()?.parse::<f64>().ok()?;
    // Days from the civil calendar, after Howard Hinnant's algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + hh * 3600.0 + mm * 60.0 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, role: Role, presence: Presence) -> Row {
        Row {
            name: name.into(),
            role,
            repo: "repo".into(),
            presence,
            detail: "ENG-2155 · branded in-place".into(),
            bg_running: 0,
            uptime: Some("12m".into()),
            session_id: None,
            branch: None,
            target: None,
            pid: None,
            asking: false,
        }
    }

    fn msg(from: &str, to: &str) -> Event {
        Event {
            ts: "2026-09-24T10:00:00.000Z".into(),
            kind: "message".into(),
            from_agent: Some(from.into()),
            to_agent: Some(to.into()),
            task_key: None,
            summary: "go".into(),
            body: None,
        }
    }

    fn fleet() -> Vec<Row> {
        vec![
            row("chief", Role::Chief, Presence::Waiting),
            row("eng-2155", Role::Worker, Presence::Working),
            row("eng-2155-review", Role::Worker, Presence::Waiting),
        ]
    }

    fn drawn(rows: &[Row], events: &[Event], ages: &[f64], w: u16, h: u16, sel: Option<&str>) -> (Buffer, String) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, rows, events, ages, sel);
        let text = (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (buf, text)
    }

    #[test]
    fn the_chief_sits_over_a_row_of_cards() {
        let (_, out) = drawn(&fleet(), &[], &[], 70, 20, None);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("◆ chief"), "the chief's card on top:\n{out}");
        let row = lines[CARDS_Y as usize + 1];
        assert!(row.contains("eng-2155"), "the workers side by side:\n{out}");
        assert!(row.contains("eng-2155-rev"), "both of them:\n{out}");
        assert!(out.contains('╭') && out.contains('╯'), "as cards, not a list:\n{out}");
    }

    #[test]
    fn a_line_runs_from_the_chief_into_every_card() {
        let (_, out) = drawn(&fleet(), &[], &[], 70, 20, None);
        let lines: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
        let l = Layout::new(70, true, 2);
        let tx = l.trunk_x() as usize;
        assert_eq!(lines[CHIEF_H as usize - 1][tx], '┬', "out of the chief:\n{out}");
        assert_eq!(lines[TRUNK_Y as usize][tx], '│', "down the trunk:\n{out}");
        for i in 0..2 {
            let c = l.card_centre(i) as usize;
            assert!("╭╮┬┼┤├".contains(lines[BUS_Y as usize][c]), "the bus turns down at {c}:\n{out}");
            assert_eq!(lines[CARDS_Y as usize][c], '┴', "and into the card:\n{out}");
        }
    }

    #[test]
    fn a_line_nothing_has_gone_along_is_dashed_and_the_busiest_is_heavy() {
        let events = vec![msg("chief", "eng-2155"), msg("chief", "eng-2155"), msg("eng-2155", "chief")];
        let ages = vec![100.0; 3];
        let (_, out) = drawn(&fleet(), &events, &ages, 70, 20, None);
        let drops: Vec<char> = out.lines().nth(DROP_Y as usize).unwrap().chars().collect();
        let l = Layout::new(70, true, 2);
        assert_eq!(drops[l.card_centre(0) as usize], '┃', "{out}");
        assert_eq!(drops[l.card_centre(1) as usize], '╎', "{out}");
        assert!(out.contains("↓2 ↑1"), "the count beside the line:\n{out}");
    }

    #[test]
    fn two_agents_talking_directly_are_joined_under_their_cards() {
        let events = vec![msg("eng-2155", "eng-2155-review")];
        let (_, out) = drawn(&fleet(), &events, &[100.0], 70, 20, None);
        let under = out.lines().nth(UNDER_Y as usize).unwrap();
        assert!(under.contains('╰') && under.contains('╯'), "{out}");
        assert!(under.contains("1▸"), "and which way it went:\n{out}");
    }

    #[test]
    fn a_message_just_sent_is_shown_travelling_and_an_old_one_is_not() {
        // Only the lines themselves: the cards carry ● for "working" too.
        let on_the_lines = |out: &str| -> bool {
            out.lines()
                .skip(CHIEF_H as usize - 1)
                .take((CARDS_Y - CHIEF_H + 1) as usize)
                .any(|l| l.contains('●') || l.contains('•'))
        };
        let events = vec![msg("chief", "eng-2155")];
        let (_, fresh) = drawn(&fleet(), &events, &[0.8], 70, 20, None);
        assert!(on_the_lines(&fresh), "in flight:\n{fresh}");
        let (_, old) = drawn(&fleet(), &events, &[60.0], 70, 20, None);
        assert!(!on_the_lines(&old), "long arrived:\n{old}");
    }

    #[test]
    fn only_a_message_is_traffic() {
        // A task moving is an event, but not one agent telling another
        // anything; counted, the busiest agent would look the chattiest.
        let mut moved = msg("chief", "eng-2155");
        moved.kind = "task".into();
        let workers: Vec<Row> = fleet().into_iter().skip(1).collect();
        let refs: Vec<&Row> = workers.iter().collect();
        let t = Traffic::count(&[moved, msg("chief", "eng-2155")], &[99.0, 99.0], Some("chief"), &refs);
        assert_eq!(t.down[0], 1, "one message, and the task move not counted");
    }

    #[test]
    fn the_line_to_a_working_agent_is_coloured_by_what_it_is_doing() {
        // zoetrope's rule: liveness reads on the structure itself.
        let (buf, _) = drawn(&fleet(), &[], &[], 70, 20, None);
        let l = Layout::new(70, true, 2);
        let working = buf.cell((l.card_centre(0), DROP_Y)).unwrap().fg;
        let waiting = buf.cell((l.card_centre(1), DROP_Y)).unwrap().fg;
        assert_eq!(working, theme::BUSY);
        assert_eq!(waiting, theme::OK);
    }

    #[test]
    fn a_line_something_just_went_along_is_lit() {
        let events = vec![msg("chief", "eng-2155-review")];
        let (buf, _) = drawn(&fleet(), &events, &[5.0], 70, 20, None);
        let l = Layout::new(70, true, 2);
        assert_eq!(buf.cell((l.card_centre(1), DROP_Y)).unwrap().fg, theme::ACCENT);
    }

    fn five() -> Vec<Row> {
        let mut rows = fleet();
        rows.push(row("third", Role::Worker, Presence::Waiting));
        rows.push(row("fourth", Role::Worker, Presence::Working));
        rows.push(row("fifth", Role::Worker, Presence::Waiting));
        rows
    }

    #[test]
    fn cards_that_do_not_fit_one_row_wrap_instead_of_running_off_the_side() {
        // Panning sideways cut the chief's own card in half and put the
        // markers on top of card text; the pane has height to spare.
        let (_, out) = drawn(&five(), &[], &[], 44, 40, None);
        for name in ["chief", "eng-2155", "third", "fourth", "fifth"] {
            assert!(out.contains(name), "{name} is on screen:\n{out}");
        }
        let top = out.lines().next().unwrap();
        assert!(top.contains('╭') && top.contains('╮'), "the chief's card is whole:\n{out}");
        for line in out.lines() {
            assert!(line.chars().count() <= 44, "nothing past the edge:\n{out}");
        }
    }

    #[test]
    fn a_wrapped_row_is_reached_down_the_left_edge_not_through_the_cards_above() {
        let (_, out) = drawn(&five(), &[], &[], 44, 40, None);
        let l = Layout::new(44, true, 5);
        assert!(l.spine && l.rows() >= 2, "this has to wrap: {} per row", l.per_row);
        let lines: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
        // From the first row's bus down to the second's, one unbroken line.
        let sx = l.spine_x as usize;
        for y in l.bus_y(0)..=l.bus_y(1) {
            assert!("│╭├┬╰".contains(lines[y as usize][sx]), "gap in the spine at row {y}:\n{out}");
        }
    }

    #[test]
    fn a_message_to_a_card_on_a_later_row_travels_down_the_spine() {
        let l = Layout::new(44, true, 5);
        let t = Traffic::default();
        let last = 4;
        assert!(l.row_of(last) > 0);
        let cells = path(&l, &t, Flight::Down(last));
        assert!(cells.iter().any(|&(x, y)| x == l.spine_x && y > l.bus_y(0)), "{cells:?}");
        assert_eq!(*cells.last().unwrap(), (l.card_centre(last), l.card_y(last)));
    }

    #[test]
    fn a_short_pane_follows_the_selection_down_and_says_there_is_more() {
        let (_, top) = drawn(&five(), &[], &[], 44, 14, Some("eng-2155"));
        assert!(top.contains("chief"), "{top}");
        assert!(top.contains('▾'), "more below:\n{top}");
        let (_, down) = drawn(&five(), &[], &[], 44, 14, Some("fifth"));
        assert!(down.contains("fifth"), "the camera went to it:\n{down}");
        assert!(down.contains('▴'), "and says what it left above:\n{down}");
    }

    #[test]
    fn nothing_is_drawn_outside_the_pane() {
        let mut rows = fleet();
        rows.push(row("third", Role::Worker, Presence::Waiting));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 30));
        let area = Rect::new(10, 3, 40, 14);
        render(&mut buf, area, &rows, &[msg("eng-2155", "third")], &[0.5], Some("third"));
        for y in 0..30 {
            for x in 0..80 {
                if !(area.x..area.x + area.width).contains(&x) || !(area.y..area.y + area.height).contains(&y) {
                    assert_eq!(buf.cell((x, y)).unwrap().symbol(), " ", "({x},{y}) is outside");
                }
            }
        }
    }

    #[test]
    fn cards_paint_no_background() {
        let (buf, _) = drawn(&fleet(), &[], &[], 70, 20, Some("eng-2155-review"));
        for cell in buf.content() {
            assert_eq!(cell.bg, Color::Reset);
        }
    }

    #[test]
    fn the_selected_card_takes_the_accent_border() {
        let (buf, _) = drawn(&fleet(), &[], &[], 70, 20, Some("eng-2155-review"));
        let l = Layout::new(70, true, 2);
        assert_eq!(buf.cell((l.card_x(1), CARDS_Y + 2)).unwrap().fg, theme::ACCENT);
        assert_eq!(buf.cell((l.card_x(0), CARDS_Y + 2)).unwrap().fg, theme::FAINT);
    }

    #[test]
    fn a_fleet_with_no_agents_yet_says_how_to_get_one() {
        let rows = vec![row("chief", Role::Chief, Presence::Waiting)];
        let (_, out) = drawn(&rows, &[], &[], 70, 20, None);
        assert!(out.contains("fleet spawn"), "{out}");
    }

    #[test]
    fn board_timestamps_read_as_seconds_since_the_epoch() {
        assert_eq!(epoch("1970-01-01T00:00:00.000Z"), Some(0.0));
        assert_eq!(epoch("2000-03-01T00:00:00Z"), Some(951_868_800.0));
        let a = epoch("2026-09-24T10:00:00.000Z").unwrap();
        let b = epoch("2026-09-24T10:00:01.500Z").unwrap();
        assert!((b - a - 1.5).abs() < 1e-6);
        assert_eq!(epoch("not a time"), None);
    }
}
