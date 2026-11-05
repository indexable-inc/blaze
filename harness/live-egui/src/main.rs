//! An egui app without a window, for harness/live_latency.py: a ledger of customers and their
//! invoices, with a filter, a sortable table, totals and charts. Every frame egui lays the whole
//! UI out, a pointer that sweeps the screen hovers and clicks it, and egui tessellates what it
//! drew into meshes, as it would for a window; nothing is put on a screen. Many of its widgets
//! are generic, as egui code tends to be (closures, `impl Display`, `Into<f64>`).
//!
//! It takes edits while it runs: blaze-live applies each patch the moment it lands, and the
//! next frame runs the new code. `answer()` is what live_latency.py's socket and burst modes
//! edit; the frame loop prints it when it changes (`answer N at_us=`), and each patch's outcome.

use std::fmt::{Debug, Display};
use std::io::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::{Color32, Pos2, Rect, RichText, Sense, Ui};

/// The size of the screen the UI is laid out for, in points.
const SCREEN: egui::Vec2 = egui::vec2(1280.0, 800.0);
/// Customers in the ledger.
const CUSTOMERS: usize = 24;
/// Invoices in the ledger.
const INVOICES: usize = 180;
/// Rows of the table shown at once.
const PAGE: usize = 40;

#[inline(never)]
fn answer() -> u64 {
    1
}

// ---- The ledger. ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Draft,
    Sent,
    Paid,
    Overdue,
}

impl Status {
    const ALL: [Status; 4] = [Status::Draft, Status::Sent, Status::Paid, Status::Overdue];

    fn color(self) -> Color32 {
        match self {
            Status::Draft => Color32::from_rgb(140, 140, 150),
            Status::Sent => Color32::from_rgb(90, 160, 250),
            Status::Paid => Color32::from_rgb(80, 200, 120),
            Status::Overdue => Color32::from_rgb(240, 90, 80),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Status::Draft => "draft",
            Status::Sent => "sent",
            Status::Paid => "paid",
            Status::Overdue => "overdue",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortBy {
    Number,
    Customer,
    Amount,
    Due,
}

impl SortBy {
    const ALL: [SortBy; 4] = [SortBy::Number, SortBy::Customer, SortBy::Amount, SortBy::Due];
}

pub struct Customer {
    pub name: String,
    pub city: &'static str,
    pub since: u32,
    pub credit_cents: i64,
}

pub struct Line {
    pub item: &'static str,
    pub quantity: u32,
    pub price_cents: i64,
}

pub struct Invoice {
    pub number: u32,
    pub customer: usize,
    pub days_left: i32,
    pub status: Status,
    pub lines: Vec<Line>,
}

impl Invoice {
    fn total_cents(&self) -> i64 {
        sum_by(&self.lines, |line| line.quantity as i64 * line.price_cents)
    }
}

/// What the table shows.
pub struct Filter {
    pub text: String,
    pub status: Option<Status>,
    pub min_cents: i64,
}

/// The whole app: the ledger, and what the UI keeps between frames.
pub struct Ledger {
    customers: Vec<Customer>,
    invoices: Vec<Invoice>,
    filter: Filter,
    sort: SortBy,
    descending: bool,
    selected: Option<usize>,
    show_charts: bool,
    page: usize,
    frames: u64,
    clicks: u64,
}

const CITIES: [&str; 8] = ["Lisbon", "Oslo", "Kyoto", "Austin", "Lagos", "Lima", "Perth", "Graz"];
const ITEMS: [&str; 10] = [
    "consulting", "licence", "support", "hardware", "training", "hosting", "audit", "design", "travel", "parts",
];
const NAMES: [&str; 12] = [
    "Acme", "Globex", "Initech", "Umbrella", "Hooli", "Vandelay", "Stark", "Wayne", "Tyrell", "Cyberdyne", "Soylent", "Wonka",
];

/// The next number of a xorshift generator: the same ledger every run.
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

impl Ledger {
    fn new() -> Ledger {
        let mut dice = 0x9e37_79b9_7f4a_7c15u64;
        let customers = (0..CUSTOMERS)
            .map(|i| Customer {
                name: format!("{} {}", NAMES[i % NAMES.len()], ["Ltd", "Inc", "GmbH", "SA"][i % 4]),
                city: CITIES[next(&mut dice) as usize % CITIES.len()],
                since: 2000 + (next(&mut dice) % 26) as u32,
                credit_cents: (next(&mut dice) % 5_000_000) as i64,
            })
            .collect();
        let invoices = (0..INVOICES)
            .map(|i| {
                let number = 1000 + i as u32;
                let lines = (0..1 + next(&mut dice) % 6)
                    .map(|_| Line {
                        item: ITEMS[next(&mut dice) as usize % ITEMS.len()],
                        quantity: 1 + (next(&mut dice) % 20) as u32,
                        price_cents: 500 + (next(&mut dice) % 90_000) as i64,
                    })
                    .collect();
                Invoice {
                    number,
                    customer: next(&mut dice) as usize % CUSTOMERS,
                    days_left: (next(&mut dice) % 90) as i32 - 30,
                    status: Status::ALL[next(&mut dice) as usize % Status::ALL.len()],
                    lines,
                }
            })
            .collect();
        Ledger {
            customers,
            invoices,
            filter: Filter { text: String::new(), status: None, min_cents: 0 },
            sort: SortBy::Number,
            descending: false,
            selected: None,
            show_charts: true,
            page: 0,
            frames: 0,
            clicks: 0,
        }
    }

    /// The invoices the filter lets through, in the order asked for.
    fn shown(&self) -> Vec<usize> {
        let text = self.filter.text.to_lowercase();
        let kept: Vec<usize> = (0..self.invoices.len())
            .filter(|&i| {
                let invoice = &self.invoices[i];
                let customer = &self.customers[invoice.customer];
                self.filter.status.is_none_or(|it| it == invoice.status)
                    && invoice.total_cents() >= self.filter.min_cents
                    && (text.is_empty() || customer.name.to_lowercase().contains(&text) || invoice.number.to_string().contains(&text))
            })
            .collect();
        let mut order = match self.sort {
            SortBy::Number => sorted_by(&kept, |&i| self.invoices[i].number),
            SortBy::Customer => sorted_by(&kept, |&i| self.customers[self.invoices[i].customer].name.clone()),
            SortBy::Amount => sorted_by(&kept, |&i| self.invoices[i].total_cents()),
            SortBy::Due => sorted_by(&kept, |&i| self.invoices[i].days_left),
        };
        if self.descending {
            order.reverse();
        }
        order.into_iter().map(|at| kept[at]).collect()
    }

    /// Ages the ledger a little: an invoice now and then is paid, or falls overdue.
    fn tick(&mut self) {
        self.frames += 1;
        if self.frames % 500 != 0 {
            return;
        }
        let at = (self.frames / 500) as usize % self.invoices.len();
        let invoice = &mut self.invoices[at];
        invoice.days_left -= 1;
        invoice.status = match invoice.status {
            Status::Sent if invoice.days_left < 0 => Status::Overdue,
            Status::Overdue if self.frames % 3 == 0 => Status::Paid,
            Status::Draft => Status::Sent,
            other => other,
        };
    }

    fn ui(&mut self, ui: &mut Ui) {
        self.tick();
        let shown = self.shown();
        egui::Panel::top("header").show(ui, |ui| self.header(ui, &shown));
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, &shown));
        egui::Panel::left("customers").show(ui, |ui| self.customer_list(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.filter_bar(ui);
            ui.separator();
            if self.show_charts {
                ui.horizontal(|ui| self.charts(ui, &shown));
                ui.separator();
            }
            self.table(ui, &shown);
        });
    }

    fn header(&mut self, ui: &mut Ui, shown: &[usize]) {
        ui.horizontal(|ui| {
            ui.heading("Ledger");
            labeled(ui, "invoices", self.invoices.len());
            labeled(ui, "shown", shown.len());
            labeled(ui, "frame", self.frames);
            labeled(ui, "clicks", self.clicks);
            ui.checkbox(&mut self.show_charts, "charts");
        });
    }

    fn footer(&mut self, ui: &mut Ui, shown: &[usize]) {
        let total = sum_by(shown, |&i| self.invoices[i].total_cents());
        let overdue = sum_by(shown, |&i| {
            let invoice = &self.invoices[i];
            if invoice.status == Status::Overdue { invoice.total_cents() } else { 0 }
        });
        ui.horizontal(|ui| {
            ui.label("Totals");
            labeled(ui, "billed", money(total));
            labeled(ui, "overdue", money(overdue));
            labeled(ui, "mean", money(if shown.is_empty() { 0 } else { total / shown.len() as i64 }));
        });
    }

    fn customer_list(&mut self, ui: &mut Ui) {
        ui.heading("Customers");
        egui::ScrollArea::vertical().id_salt("customer list").show(ui, |ui| {
            for (i, customer) in self.customers.iter().enumerate() {
                let owed = sum_by(&self.invoices, |it| {
                    if it.customer == i && it.status != Status::Paid { it.total_cents() } else { 0 }
                });
                let response = ui.selectable_label(
                    self.filter.text == customer.name,
                    format!("{} ({}, since {})", customer.name, customer.city, customer.since),
                );
                if response.clicked() {
                    self.filter.text = customer.name.clone();
                    self.clicks += 1;
                }
                stat_bar(ui, "owed", owed as f64 / 100.0, customer.credit_cents.max(1) as f64 / 100.0);
            }
        });
    }

    fn filter_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Find");
            ui.text_edit_singleline(&mut self.filter.text);
            if ui.small_button("clear").clicked() {
                self.filter.text.clear();
                self.clicks += 1;
            }
            option_picker(ui, "status", &mut self.filter.status, &Status::ALL, |it| it.name().to_owned());
            enum_picker(ui, "sort", &mut self.sort, &SortBy::ALL);
            ui.checkbox(&mut self.descending, "descending");
            for (label, cents) in [("any", 0), ("> 1k", 100_000), ("> 10k", 1_000_000)] {
                if ui.selectable_label(self.filter.min_cents == cents, label).clicked() {
                    self.filter.min_cents = cents;
                    self.clicks += 1;
                }
            }
        });
    }

    fn charts(&mut self, ui: &mut Ui, shown: &[usize]) {
        let amounts: Vec<f64> = shown.iter().map(|&i| self.invoices[i].total_cents() as f64 / 100.0).collect();
        let days: Vec<i32> = shown.iter().map(|&i| self.invoices[i].days_left).collect();
        ui.vertical(|ui| {
            ui.label("Amounts");
            histogram(ui, &amounts, 24);
        });
        ui.vertical(|ui| {
            ui.label("Days left");
            histogram(ui, &days, 24);
        });
        ui.vertical(|ui| {
            for status in Status::ALL {
                let count = shown.iter().filter(|&&i| self.invoices[i].status == status).count();
                ui.horizontal(|ui| {
                    ui.colored_label(status.color(), status.name());
                    stat_bar(ui, "count", count as u32, shown.len().max(1) as f64);
                });
            }
        });
    }

    fn table(&mut self, ui: &mut Ui, shown: &[usize]) {
        let pages = shown.len().div_ceil(PAGE).max(1);
        self.page = self.page.min(pages - 1);
        ui.horizontal(|ui| {
            if ui.small_button("<").clicked() && self.page > 0 {
                self.page -= 1;
                self.clicks += 1;
            }
            labeled(ui, "page", format!("{} of {pages}", self.page + 1));
            if ui.small_button(">").clicked() && self.page + 1 < pages {
                self.page += 1;
                self.clicks += 1;
            }
        });
        let rows: Vec<usize> = shown.iter().skip(self.page * PAGE).take(PAGE).copied().collect();
        let mut picked = None;
        egui::ScrollArea::vertical().id_salt("invoices").show(ui, |ui| {
            grid(ui, "invoice table", &["number", "customer", "lines", "total", "due", "status"], &rows, |ui, &i| {
                if invoice_row(ui, &self.invoices[i], &self.customers, self.selected == Some(i)) {
                    picked = Some(i);
                }
            });
        });
        if let Some(i) = picked {
            self.selected = if self.selected == Some(i) { None } else { Some(i) };
            self.clicks += 1;
        }
        if let Some(i) = self.selected {
            ui.separator();
            self.details(ui, i);
        }
    }

    fn details(&self, ui: &mut Ui, i: usize) {
        let invoice = &self.invoices[i];
        ui.collapsing(format!("Invoice {}", invoice.number), |ui| {
            grid(ui, "lines", &["item", "quantity", "price", "amount"], &invoice.lines, |ui, line| {
                ui.label(line.item);
                ui.label(line.quantity.to_string());
                ui.label(money(line.price_cents));
                ui.label(money(line.quantity as i64 * line.price_cents));
            });
        });
    }
}

// ---- Widgets. ------------------------------------------------------------------------------

/// `name: value`, for anything that prints.
#[inline(never)]
fn labeled<T: Display>(ui: &mut Ui, name: &str, value: T) -> egui::Response {
    ui.label(format!("{name}: {value}"))
}

/// A bar `value` of `max` long, with the number after it.
#[inline(never)]
fn stat_bar<T: Into<f64>>(ui: &mut Ui, name: &str, value: T, max: f64) {
    let value: f64 = value.into();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(160.0, 12.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Color32::from_gray(50));
    let filled = (value / max).clamp(0.0, 1.0) as f32;
    let bar = Rect::from_min_size(rect.min, egui::vec2(rect.width() * filled, rect.height()));
    let color = if filled > 0.9 { Color32::from_rgb(240, 90, 80) } else { Color32::from_rgb(90, 160, 250) };
    painter.rect_filled(bar, 3.0, color);
    if response.hovered() {
        response.on_hover_text(format!("{name}: {value:.1} of {max:.1}"));
    }
}

/// A combo box over every value of an enum.
fn enum_picker<E: Copy + PartialEq + Debug>(ui: &mut Ui, label: &str, value: &mut E, all: &[E]) {
    egui::ComboBox::from_label(label).selected_text(format!("{value:?}")).show_ui(ui, |ui| {
        for it in all {
            ui.selectable_value(value, *it, format!("{it:?}"));
        }
    });
}

/// A combo box over an optional value: "all", or one of `all`.
fn option_picker<E: Copy + PartialEq, F: Fn(&E) -> String>(ui: &mut Ui, label: &str, value: &mut Option<E>, all: &[E], name: F) {
    let shown = value.as_ref().map_or("all".to_owned(), &name);
    egui::ComboBox::from_label(label).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(value, None, "all");
        for it in all {
            ui.selectable_value(value, Some(*it), name(it));
        }
    });
}

/// A striped grid with a header row, a row of `row` for each of `rows`.
fn grid<T, F: FnMut(&mut Ui, &T)>(ui: &mut Ui, id: &str, header: &[&str], rows: &[T], mut row: F) {
    egui::Grid::new(id).num_columns(header.len()).striped(true).show(ui, |ui| {
        for title in header {
            ui.label(RichText::new(*title).strong());
        }
        ui.end_row();
        for it in rows {
            row(ui, it);
            ui.end_row();
        }
    });
}

/// One invoice as a row of the table. Says whether it was clicked.
fn invoice_row(ui: &mut Ui, invoice: &Invoice, customers: &[Customer], selected: bool) -> bool {
    let clicked = ui.selectable_label(selected, format!("#{}", invoice.number)).clicked();
    ui.horizontal(|ui| {
        ui.set_min_width(140.0);
        ui.label(&customers[invoice.customer].name);
    });
    ui.label(invoice.lines.len().to_string());
    ui.monospace(money(invoice.total_cents()));
    let due = match invoice.days_left {
        d if d < 0 => RichText::new(format!("{} days late", -d)).color(Color32::from_rgb(240, 90, 80)),
        0 => RichText::new("today"),
        d => RichText::new(format!("in {d} days")),
    };
    ui.label(due);
    ui.colored_label(invoice.status.color(), invoice.status.name());
    clicked
}

/// Bars of how many of `values` fall in each of `buckets` equal ranges.
fn histogram<T: Copy + Into<f64>>(ui: &mut Ui, values: &[T], buckets: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(240.0, 60.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::from_gray(35));
    if values.is_empty() {
        return;
    }
    let numbers: Vec<f64> = values.iter().map(|it| (*it).into()).collect();
    let (low, high) = numbers.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = (high - low).max(1e-9);
    let mut counts = vec![0usize; buckets];
    for v in &numbers {
        let at = (((v - low) / span) * buckets as f64) as usize;
        counts[at.min(buckets - 1)] += 1;
    }
    let tallest = counts.iter().copied().max().unwrap_or(1).max(1) as f32;
    let width = rect.width() / buckets as f32;
    for (i, count) in counts.into_iter().enumerate() {
        let height = rect.height() * count as f32 / tallest;
        let bar = Rect::from_min_max(
            Pos2::new(rect.min.x + i as f32 * width + 1.0, rect.max.y - height),
            Pos2::new(rect.min.x + (i + 1) as f32 * width - 1.0, rect.max.y),
        );
        painter.rect_filled(bar, 1.0, Color32::from_rgb(120, 180, 240));
    }
}

// ---- Plain functions. ----------------------------------------------------------------------

/// The sum of `f` over `items`.
fn sum_by<T, F: Fn(&T) -> i64>(items: &[T], f: F) -> i64 {
    items.iter().map(f).sum()
}

/// The positions of `items` in the order of `key`.
fn sorted_by<T, K: Ord, F: Fn(&T) -> K>(items: &[T], key: F) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| key(&items[i]));
    order
}

/// Cents as money: 1,234.56.
fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{sign}{grouped}.{:02}", cents % 100)
}

// ---- The frame loop. -----------------------------------------------------------------------

fn at_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_micros())
}

/// What a person at the window would do this frame: the pointer sweeps the screen in a
/// Lissajous curve, and clicks every 90th frame.
fn input(frame: u64, since: Duration) -> egui::RawInput {
    let t = frame as f32 * 0.013;
    let pos = Pos2::new(
        SCREEN.x * (0.5 + 0.45 * (t * 1.3).sin()),
        SCREEN.y * (0.5 + 0.45 * (t * 0.7).cos()),
    );
    let mut events = vec![egui::Event::PointerMoved(pos)];
    if frame % 90 == 0 {
        for pressed in [true, false] {
            events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }
    egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
        time: Some(since.as_secs_f64()),
        events,
        ..Default::default()
    }
}

fn main() {
    let live = match blaze_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    // Applied the moment it arrives, in the middle of a frame or between two, as the Game of
    // Life window does.
    live.apply_as_they_come(|outcome| {
        println!("outcome: {outcome} at_us={}", at_us());
        let _ = std::io::stdout().flush();
    });
    let ctx = egui::Context::default();
    let mut ledger = Ledger::new();
    let start = Instant::now();
    let mut seen = None;
    let mut slowest = Duration::ZERO;
    println!("ready");
    for frame in 0u64.. {
        let began = Instant::now();
        let mut output = ctx.run_ui(input(frame, start.elapsed()), |ui| ledger.ui(ui));
        let meshes = ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        std::hint::black_box(meshes.len());
        // No window to upload the font atlas to.
        output.drop_without_applying_deltas();
        slowest = slowest.max(began.elapsed());
        let now = answer();
        if seen != Some(now) {
            println!("answer {now} at_us={}", at_us());
            seen = Some(now);
        }
        let _ = std::io::stdout().flush();
        if frame % 1_000 == 0 {
            eprintln!("frame {frame}: {} meshes, slowest frame {slowest:?}, {} clicks", meshes.len(), ledger.clicks);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
