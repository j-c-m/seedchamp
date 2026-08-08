//! List sort screens and criteria (config-driven TUI sort).

/// One sort key in a config-driven screen `order` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortCriterion {
    /// RUN off first (`want_start == false` before true).
    OffFirst,
    DownRateDesc,
    UpRateDesc,
    AddedDesc,
    NameAsc,
    IdAsc,
}

impl SortCriterion {
    /// Config `order` token: `off_first` | `down_rate_desc` | `up_rate_desc` |
    /// `added_desc` | `name_asc` | `id_asc` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off_first" => Some(Self::OffFirst),
            "down_rate_desc" => Some(Self::DownRateDesc),
            "up_rate_desc" => Some(Self::UpRateDesc),
            "added_desc" => Some(Self::AddedDesc),
            "name_asc" => Some(Self::NameAsc),
            "id_asc" => Some(Self::IdAsc),
            _ => None,
        }
    }
}

/// One list sort screen (from `tui.screens` or built-in defaults).
#[derive(Debug, Clone)]
pub struct ListSortScreen {
    pub key: String,
    pub label: String,
    pub order: Vec<SortCriterion>,
}

impl ListSortScreen {
    pub fn from_config(s: &seedchamp_engine::TuiSortScreen) -> Self {
        let mut order: Vec<SortCriterion> = s
            .order
            .iter()
            .filter_map(|t| SortCriterion::parse(t))
            .collect();
        if order.is_empty() {
            order = vec![
                SortCriterion::OffFirst,
                SortCriterion::DownRateDesc,
                SortCriterion::UpRateDesc,
                SortCriterion::AddedDesc,
                SortCriterion::NameAsc,
            ];
        }
        Self {
            key: s.key.clone(),
            label: if s.label.trim().is_empty() {
                s.key.clone()
            } else {
                s.label.clone()
            },
            order,
        }
    }

    pub fn needs_live_rates(&self) -> bool {
        self.order
            .iter()
            .any(|c| matches!(c, SortCriterion::DownRateDesc | SortCriterion::UpRateDesc))
    }
}

/// Active sort: index into the config-driven screen list.
#[derive(Debug, Clone)]
pub struct ListSort {
    pub screens: Vec<ListSortScreen>,
    pub index: usize,
}

impl ListSort {
    pub fn from_tui_config(tui: &seedchamp_engine::TuiConfig) -> Self {
        let screens: Vec<ListSortScreen> = tui
            .sort_screens()
            .iter()
            .map(ListSortScreen::from_config)
            .collect();
        let screens = if screens.is_empty() {
            seedchamp_engine::default_sort_screens()
                .iter()
                .map(ListSortScreen::from_config)
                .collect()
        } else {
            screens
        };
        let index = tui
            .default_screen_index()
            .min(screens.len().saturating_sub(1));
        Self { screens, index }
    }

    pub fn current(&self) -> &ListSortScreen {
        &self.screens[self.index.min(self.screens.len().saturating_sub(1))]
    }

    pub fn cycle(&mut self) {
        if self.screens.is_empty() {
            return;
        }
        self.index = (self.index + 1) % self.screens.len();
    }

    pub fn set_by_key(&mut self, s: &str) -> bool {
        let key = s.trim().to_ascii_lowercase();
        if let Some(i) = self
            .screens
            .iter()
            .position(|sc| sc.key.eq_ignore_ascii_case(&key) || sc.label.eq_ignore_ascii_case(&key))
        {
            self.index = i;
            true
        } else {
            false
        }
    }

    pub fn label(&self) -> &str {
        &self.current().label
    }

    pub fn screen_num(&self) -> u8 {
        // Numeric key if the screen key is a number; else 1-based index.
        self.current()
            .key
            .parse::<u8>()
            .unwrap_or((self.index as u8).saturating_add(1))
    }

    pub fn needs_live_rates(&self) -> bool {
        self.current().needs_live_rates()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_tokens() {
        assert_eq!(
            SortCriterion::parse("off_first"),
            Some(SortCriterion::OffFirst)
        );
        assert_eq!(
            SortCriterion::parse("DOWN_RATE_DESC"),
            Some(SortCriterion::DownRateDesc)
        );
        assert_eq!(
            SortCriterion::parse("  id_asc  "),
            Some(SortCriterion::IdAsc)
        );
    }

    #[test]
    fn parse_rejects_synonyms() {
        assert!(SortCriterion::parse("stopped_first").is_none());
        assert!(SortCriterion::parse("rate_desc").is_none());
        assert!(SortCriterion::parse("name").is_none());
        assert!(SortCriterion::parse("id").is_none());
        assert!(SortCriterion::parse("created_desc").is_none());
    }
}
