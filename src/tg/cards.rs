//! Pure screen renderers. Every message the bot sends is built here from
//! plain data, which is what keeps one card grammar across the whole bot and
//! makes every screen unit-testable (see docs/S2-UX.md).

use crate::tg::{Button, Card};

fn b(label: &str, callback: &str) -> Button {
    Button::new(label, callback)
}

/// `0x71C7…9A3f`
pub fn short_addr(addr: &str) -> String {
    if addr.len() <= 12 {
        return addr.to_string();
    }
    format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
}

/// Humanises 1e18-scaled units with thousands separators and 4 decimals.
pub fn fmt_units(raw: &alloy::primitives::U256) -> String {
    let e18 = alloy::primitives::U256::from(10).pow(alloy::primitives::U256::from(18));
    let whole = raw / e18;
    let frac = raw % e18;
    let frac_4 = frac / alloy::primitives::U256::from(10).pow(alloy::primitives::U256::from(14));
    let whole_str = group_thousands(&whole.to_string());
    format!("{whole_str}.{frac_4:04}")
}

fn group_thousands(s: &str) -> String {
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

pub fn fmt_price(raw: &alloy::primitives::U256) -> String {
    if raw.is_zero() {
        return "0".to_string();
    }
    let e18 = alloy::primitives::U256::from(10).pow(alloy::primitives::U256::from(18));
    if *raw >= e18 {
        fmt_units(raw)
    } else {
        // Small prices: show the first significant digits in scientific-ish form.
        let mut s = raw.to_string();
        let digits = s.trim_end_matches('0');
        let exp = s.len() - digits.len();
        s = digits.to_string();
        let lead = s.chars().take(4).collect::<String>();
        let rest: String = s.chars().skip(4).take(2).collect();
        format!("{lead}.{rest}e-{exp}")
    }
}

// ----------------------------------------------------------------- headers

pub fn first_contact() -> Card {
    Card {
        text: "BaseVantage\nProfessional trading on Base — from this chat.\n\n\
⚠️ Trading is risky. You can lose money; tokens can be scams.\n\
This bot checks tax, honeypot and price-impact before every trade and\n\
refuses what it cannot vouch for. It never moves funds on its own.\n\n\
Your wallet is stored encrypted. We will show the key once — save it."
            .to_string(),
        rows: vec![vec![
            b("Create Wallet", "w:create"),
            b("Import Wallet", "w:import"),
            b("Explore First", "menu"),
        ]],
    }
}

pub fn menu(address: Option<&str>, balance_line: &str, mode_line: &str) -> Card {
    let wallet_line = match address {
        Some(a) => format!("wallet  {} · {balance_line}", short_addr(a)),
        None => "wallet  not set up — create or import one".to_string(),
    };
    Card {
        text: format!(
            "BaseVantage · menu\n{wallet_line}\nmode    {mode_line}\nnetwork Base · engine ready"
        ),
        rows: vec![
            vec![
                b("Trade", "nav:trade"),
                b("Positions", "nav:pos"),
                b("Orders", "nav:ord"),
            ],
            vec![
                b("Watchlist", "nav:wl"),
                b("Wallet", "nav:wal"),
                b("Settings", "nav:set"),
            ],
            vec![b("Help", "nav:help")],
        ],
    }
}

pub fn help(mode_line: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · help\n\
paste a token address (0x…) to open its trade card.\n\
every trade shows a review card first; nothing sends without Confirm.\n\
target orders fire only inside your limit: buy ≤ limit, sell ≥ limit.\n\
refusals are explained; the engine never guesses.\n\n\
mode {mode_line}\n\
security: keys encrypted; export needs two confirms.\n\
invariants tested — never \"perfect\"."
        ),
        rows: vec![vec![b("← Menu", "menu")]],
    }
}

// ------------------------------------------------------------- token card

pub struct TokenCardData {
    pub symbol: String,
    pub price_line: String,
    pub pools_line: String,
    pub dossier_line: String,
    pub impact_line: String,
    pub verdict_line: String,
    pub blocked: bool,
    pub block_reason: String,
    pub holds: bool,
}

pub fn token_card(addr: &str, d: &TokenCardData) -> Card {
    let text = format!(
        "BaseVantage · token\n{} ({}) · {}\npools   {}\ndossier {}\nimpact  {}\nverdict {}",
        d.symbol,
        short_addr(addr),
        d.price_line,
        d.pools_line,
        d.dossier_line,
        d.impact_line,
        d.verdict_line
    );
    if d.blocked {
        return Card {
            text: format!("{text}\n\n🚫 blocked — {}", d.block_reason),
            rows: vec![vec![b("Why?", "nav:help"), b("← Menu", "menu")]],
        };
    }
    let mut rows = vec![
        vec![
            b("Buy 0.05", "t:buy:0.05"),
            b("Buy 0.1", "t:buy:0.1"),
            b("Buy 0.25", "t:buy:0.25"),
        ],
        vec![b("Buy custom ▸", "t:buyx"), b("Target ▸", "t:tgt")],
    ];
    if d.holds {
        rows.insert(2, vec![b("Sell ▸", "t:sell")]);
    }
    rows.push(vec![
        b("⟳ Refresh", &format!("tok:{addr}")),
        b("← Menu", "menu"),
    ]);
    Card { text, rows }
}

// --------------------------------------------------------------- sizing

pub fn buy_sizing(symbol: &str, spend_line: &str, out_line: &str, slippage_pct: f64) -> Card {
    Card {
        text: format!(
            "BaseVantage · buy {symbol}\nspending  {spend_line}\nest. out  {out_line}\nslippage  {slippage_pct:.1}%"
        ),
        rows: vec![
            vec![
                b("◂ 0.05", "t:buy:0.05"),
                b("0.1 ▸", "t:buy:0.1"),
                b("Custom ▸", "t:buyx"),
            ],
            vec![
                b("Review", "ord:review"),
                b("← Token", "back:token"),
                b("← Menu", "menu"),
            ],
        ],
    }
}

pub fn sell_position(
    symbol: &str,
    held_line: &str,
    entry_line: &str,
    verdict_line: &str,
    addr: &str,
) -> Card {
    Card {
        text: format!(
            "BaseVantage · position {symbol}\nheld    {held_line}\n{entry_line}\nverdict {verdict_line}"
        ),
        rows: vec![
            vec![
                b("Sell 25%", "t:sellp:25"),
                b("Sell 50%", "t:sellp:50"),
                b("Sell 75%", "t:sellp:75"),
                b("Sell 100%", "t:sellp:100"),
            ],
            vec![
                b("Custom ▸", "t:sellx"),
                b("TOKEN ▸", &format!("tok:{addr}")),
                b("← Menu", "menu"),
            ],
        ],
    }
}

// ---------------------------------------------------------------- review

pub struct ReviewData {
    pub title: String,
    pub body: String,
    pub mode_observe: bool,
}

pub fn review(id: u64, d: &ReviewData) -> Card {
    let tail = if d.mode_observe {
        "verdict allow · mode observe (nothing will be sent)\nconfirm window 60s"
    } else {
        "verdict allow · mode execute (real funds)\nconfirm window 60s"
    };
    Card {
        text: format!("BaseVantage · REVIEW · {}\n{}\n{}", d.title, d.body, tail),
        rows: vec![vec![
            b("Confirm", &format!("ord:ok:{id}")),
            b("Cancel", &format!("ord:cancel:{id}")),
        ]],
    }
}

pub fn executing(title: &str) -> Card {
    Card {
        text: format!("BaseVantage · executing · {title}\nsending transaction…"),
        rows: vec![],
    }
}

pub struct ReceiptData {
    pub title: String,
    pub fill_line: String,
    pub impact_line: String,
    pub tx_hash: String,
    pub verdict_line: String,
    pub token_addr: String,
}

pub fn receipt(d: &ReceiptData) -> Card {
    Card {
        text: format!(
            "BaseVantage · receipt · {}\nfilled  {}\nimpact  {}\ntx      {}\nverdict {}",
            d.title,
            d.fill_line,
            d.impact_line,
            short_addr(&d.tx_hash),
            d.verdict_line
        ),
        rows: vec![vec![
            b("Positions", "nav:pos"),
            b("TOKEN ▸", &format!("tok:{}", d.token_addr)),
            b("← Menu", "menu"),
        ]],
    }
}

pub fn refusal(title: &str, reason: &str, guidance: &str, token_addr: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · refused\n{title} — not sent\nreason  {reason}\nwhat now  {guidance}\nverdict refused (your funds were not touched)"
        ),
        rows: vec![vec![
            b("TOKEN ▸", &format!("tok:{token_addr}")),
            b("← Menu", "menu"),
        ]],
    }
}

pub fn observe_decline(title: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · observe mode\n{title} — not sent: observe mode never sends transactions.\nswitch mode in Settings to execute for real."
        ),
        rows: vec![vec![b("Settings", "nav:set"), b("← Menu", "menu")]],
    }
}

pub fn expired(what: &str, token_addr: &str) -> Card {
    Card {
        text: format!("BaseVantage · expired\n{what} — confirm window closed; nothing was sent."),
        rows: vec![vec![
            b("Start over", &format!("tok:{token_addr}")),
            b("← Menu", "menu"),
        ]],
    }
}

pub fn internal_error(incident: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · error\nsomething went wrong — nothing was sent.\nincident {incident}\ntry again; if it persists, check /settings → network."
        ),
        rows: vec![vec![b("← Menu", "menu")]],
    }
}

// -------------------------------------------------------------- positions

pub fn positions(rows_text: &str, total_line: &str, addr_for_first: Option<&str>) -> Card {
    let mut rows = Vec::new();
    if let Some(a) = addr_for_first {
        rows.push(vec![b("Open ▸", &format!("posopen:{a}"))]);
    }
    rows.push(vec![b("⟳ Refresh", "nav:pos"), b("← Menu", "menu")]);
    Card {
        text: format!("BaseVantage · positions\n{rows_text}\n{total_line}"),
        rows,
    }
}

// ----------------------------------------------------------------- orders

pub fn orders_list(listing: &str) -> Card {
    Card {
        text: format!("BaseVantage · orders\n{listing}"),
        rows: vec![
            vec![b("New Target Order", "ord:new"), b("⟳ Refresh", "nav:ord")],
            vec![b("← Menu", "menu")],
        ],
    }
}

pub fn order_detail(
    id: u64,
    bound_line: &str,
    status_line: &str,
    token_addr: &str,
    watching: bool,
) -> Card {
    let mut rows = Vec::new();
    if watching {
        rows.push(vec![b("Cancel Order", &format!("ord:cancel:{id}"))]);
    }
    rows.push(vec![
        b("TOKEN ▸", &format!("tok:{token_addr}")),
        b("← Menu", "menu"),
    ]);
    Card {
        text: format!("BaseVantage · order #{id}\n{bound_line}\nstatus  {status_line}"),
        rows,
    }
}

pub fn target_review(id: u64, direction: &str, body: &str, mode_observe: bool) -> Card {
    let tail = if mode_observe {
        "verdict  allow · mode observe (fires as a review prompt)"
    } else {
        "verdict  allow · mode execute (fires automatically)"
    };
    Card {
        text: format!("BaseVantage · REVIEW · target {direction}\n{body}\n{tail}"),
        rows: vec![vec![
            b("Place Order", &format!("ord:place:{id}")),
            b("Cancel", &format!("ord:cancel:{id}")),
        ]],
    }
}

pub fn target_filled(
    direction: &str,
    headline: &str,
    fill_line: &str,
    bound_line: &str,
    tx_hash: &str,
    token_addr: &str,
) -> Card {
    Card {
        text: format!(
            "BaseVantage · target order filled\n{direction} {headline}\n{fill_line}\n{bound_line}\ntx      {}",
            short_addr(tx_hash)
        ),
        rows: vec![vec![
            b("TOKEN ▸", &format!("tok:{token_addr}")),
            b("Orders", "nav:ord"),
            b("← Menu", "menu"),
        ]],
    }
}

pub fn target_prompt(direction: &str, headline: &str, bound_line: &str, id: u64) -> Card {
    Card {
        text: format!(
            "BaseVantage · target reached\n{direction} {headline}\n{bound_line}\nmode observe — review to send, or cancel."
        ),
        rows: vec![vec![
            b("Review & Confirm", &format!("ord:fire:{id}")),
            b("Cancel", &format!("ord:cancel:{id}")),
        ]],
    }
}

// -------------------------------------------------------------- watchlist

pub fn watchlist(listing: &str, count: usize) -> Card {
    Card {
        text: format!("BaseVantage · watchlist  {count}/50\n{listing}"),
        rows: vec![
            vec![b("Enroll token ▸", "wl:add"), b("⟳ Refresh", "nav:wl")],
            vec![b("← Menu", "menu")],
        ],
    }
}

// ----------------------------------------------------------------- wallet

pub fn wallet(address: Option<&str>, balance_line: &str, created_line: &str) -> Card {
    let (addr_line, rows) = match address {
        Some(a) => (
            format!("address {}   [Show full below]", short_addr(a)),
            vec![
                vec![b("Deposit", "w:dep"), b("Withdraw ▸", "w:wd")],
                vec![b("Show full", "w:full"), b("Export Key ⚠", "w:exp")],
                vec![b("Import Key", "w:import"), b("New Wallet", "w:create")],
                vec![b("← Menu", "menu")],
            ],
        ),
        None => (
            "address none — create or import a wallet".to_string(),
            vec![
                vec![
                    b("Create Wallet", "w:create"),
                    b("Import Wallet", "w:import"),
                ],
                vec![b("← Menu", "menu")],
            ],
        ),
    };
    Card {
        text: format!(
            "BaseVantage · wallet\n{addr_line}\nbalance {balance_line}\nstored  {created_line}"
        ),
        rows,
    }
}

pub fn deposit(address: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · deposit\n{address}\n\nsend only Base-network assets to this address."
        ),
        rows: vec![vec![b("← Wallet", "nav:wal"), b("← Menu", "menu")]],
    }
}

pub fn export_warning() -> Card {
    Card {
        text: "BaseVantage · ⚠ export key\nthis shows your private key in chat.\n\
anyone with it controls your funds. Make sure nobody is watching.\n\
we will not show it again after this."
            .to_string(),
        rows: vec![vec![b("Yes, I understand", "w:exp2"), b("No", "nav:wal")]],
    }
}

pub fn export_reveal(key_hex: &str) -> Card {
    Card {
        text: format!(
            "BaseVantage · ⚠ your private key (shown once)\n{key_hex}\n\nsave it now; this message is never repeated."
        ),
        rows: vec![vec![b("← Wallet", "nav:wal")]],
    }
}

// --------------------------------------------------------------- settings

pub struct SettingsView {
    pub slippage_pct: f64,
    pub gas_label: &'static str,
    pub confirm_on: bool,
    pub mode_line: String,
}

pub fn settings(s: &SettingsView) -> Card {
    let toggle = if s.confirm_on { "on" } else { "off" };
    Card {
        text: format!(
            "BaseVantage · settings\nslippage     {:.1}%\ngas profile  {}\nconfirm step {}\nmode         {}",
            s.slippage_pct, s.gas_label, toggle, s.mode_line
        ),
        rows: vec![
            vec![b("◂ slippage", "set:slip:-"), b("slippage ▸", "set:slip:+")],
            vec![b("◂ gas", "set:gas:-"), b("gas ▸", "set:gas:+")],
            vec![
                b("toggle confirm", "set:confirm"),
                b("Switch mode ⚠", "set:mode"),
            ],
            vec![b("← Menu", "menu")],
        ],
    }
}

pub fn execute_confirm() -> Card {
    Card {
        text: "BaseVantage · ⚠ switch mode\nswitching to execute means Confirm buttons send real\n\
transactions with real funds on Base. Checkpoints stay: review card,\n\
min-out floors, safety gates. You can switch back anytime.\n\n\
(enabling execute may be limited to an allowlist during soft launch)"
            .to_string(),
        rows: vec![vec![
            b("Enable Execute", "set:mode:on"),
            b("Keep Observe", "nav:set"),
        ]],
    }
}
