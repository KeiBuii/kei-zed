use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use gpui::{EntityId, Global, Image};

use crate::document::PageColors;

/// Memory that rendered pages of all open documents may use together. A rendered page is kept
/// decoded as 4 bytes per pixel, so an A4 page at 100% zoom on a Retina display takes about
/// 14 MB, and four times that at 200%.
const MEMORY_BUDGET: usize = 200 * 1024 * 1024;

/// The most recently used pages are kept even over budget. Visible pages are used on every
/// frame, so evicting them would make them render again in a loop.
const MIN_KEPT_PAGES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub view: EntityId,
    pub page_index: usize,
}

#[derive(Clone)]
pub struct CachedPage {
    pub scale: f32,
    pub colors: PageColors,
    pub image: Arc<Image>,
    bytes: usize,
}

impl CachedPage {
    /// `width` and `height` are the size of the rendered image, in pixels.
    pub fn new(scale: f32, colors: PageColors, image: Arc<Image>, width: f32, height: f32) -> Self {
        let bytes = (width.max(0.0).ceil() as usize) * (height.max(0.0).ceil() as usize) * 4;
        Self {
            scale,
            colors,
            image,
            bytes,
        }
    }
}

/// Rendered pages of every document view, shared so that the memory they use stays bounded
/// however many documents are open. Least recently used pages are evicted first.
///
/// Methods that remove pages return their images, so that the caller can free the decoded copy
/// that gpui keeps for each with [`Image::remove_asset`].
pub struct PageCache {
    budget: usize,
    pages: HashMap<PageKey, CachedPage>,
    /// Most recently used last.
    use_order: VecDeque<PageKey>,
    total_bytes: usize,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::with_budget(MEMORY_BUDGET)
    }
}

impl Global for PageCache {}

impl PageCache {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            pages: HashMap::default(),
            use_order: VecDeque::new(),
            total_bytes: 0,
        }
    }

    #[cfg(test)]
    fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn page_count(&self, view: EntityId) -> usize {
        self.pages.keys().filter(|key| key.view == view).count()
    }

    /// Returns the page and marks it as the most recently used.
    pub fn use_page(&mut self, key: PageKey) -> Option<CachedPage> {
        let page = self.pages.get(&key)?.clone();
        self.mark_used(key);
        Some(page)
    }

    pub fn insert(&mut self, key: PageKey, page: CachedPage) -> Vec<Arc<Image>> {
        let mut removed = Vec::new();
        self.total_bytes += page.bytes;
        if let Some(replaced) = self.pages.insert(key, page) {
            self.total_bytes -= replaced.bytes;
            removed.push(replaced.image);
        }
        self.mark_used(key);

        while self.total_bytes > self.budget && self.use_order.len() > MIN_KEPT_PAGES {
            let Some(evicted) = self.use_order.pop_front() else {
                break;
            };
            if let Some(page) = self.pages.remove(&evicted) {
                self.total_bytes -= page.bytes;
                removed.push(page.image);
            }
        }
        removed
    }

    pub fn remove_view(&mut self, view: EntityId) -> Vec<Arc<Image>> {
        self.use_order.retain(|key| key.view != view);
        let keys: Vec<PageKey> = self
            .pages
            .keys()
            .filter(|key| key.view == view)
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|key| self.pages.remove(&key))
            .map(|page| {
                self.total_bytes -= page.bytes;
                page.image
            })
            .collect()
    }

    fn mark_used(&mut self, key: PageKey) {
        if self.use_order.back() == Some(&key) {
            return;
        }
        self.use_order.retain(|used| *used != key);
        self.use_order.push_back(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ImageFormat;

    const MB: usize = 1024 * 1024;

    fn key(view: u64, page_index: usize) -> PageKey {
        PageKey {
            view: EntityId::from(view),
            page_index,
        }
    }

    /// A page whose decoded image takes `megabytes` MB.
    fn page(megabytes: usize) -> CachedPage {
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, Vec::new()));
        CachedPage::new(
            1.0,
            PageColors::PAPER,
            image,
            1024.0,
            (megabytes * 256) as f32,
        )
    }

    #[test]
    fn test_evicts_least_recently_used_pages_over_budget() {
        let mut cache = PageCache::with_budget(100 * MB);
        for page_index in 0..5 {
            assert!(cache.insert(key(1, page_index), page(20)).is_empty());
            if page_index == 1 {
                // Page 0 becomes more recently used than page 1.
                assert!(cache.use_page(key(1, 0)).is_some());
            }
        }

        // A sixth 20 MB page is over the 100 MB budget, so the least recently used page goes.
        assert_eq!(cache.insert(key(1, 5), page(20)).len(), 1);
        assert_eq!(cache.total_bytes(), 100 * MB);
        assert!(cache.use_page(key(1, 1)).is_none());
        assert!(cache.use_page(key(1, 0)).is_some());
        assert_eq!(cache.page_count(EntityId::from(1)), 5);
    }

    #[test]
    fn test_budget_is_shared_by_all_views() {
        let mut cache = PageCache::with_budget(100 * MB);
        for page_index in 0..5 {
            cache.insert(key(1, page_index), page(20));
        }
        cache.insert(key(2, 0), page(20));

        assert_eq!(cache.page_count(EntityId::from(1)), 4);
        assert_eq!(cache.page_count(EntityId::from(2)), 1);
        assert!(cache.use_page(key(1, 0)).is_none());
    }

    #[test]
    fn test_keeps_recent_pages_over_budget() {
        let mut cache = PageCache::with_budget(10 * MB);
        for page_index in 0..MIN_KEPT_PAGES + 2 {
            cache.insert(key(1, page_index), page(30));
        }
        assert_eq!(cache.page_count(EntityId::from(1)), MIN_KEPT_PAGES);
        assert_eq!(cache.total_bytes(), MIN_KEPT_PAGES * 30 * MB);
    }

    #[test]
    fn test_replacing_a_page_returns_the_old_image() {
        let mut cache = PageCache::with_budget(100 * MB);
        cache.insert(key(1, 0), page(10));
        let removed = cache.insert(key(1, 0), page(30));

        assert_eq!(removed.len(), 1);
        assert_eq!(cache.total_bytes(), 30 * MB);
        assert_eq!(cache.page_count(EntityId::from(1)), 1);
    }

    #[test]
    fn test_removes_all_pages_of_a_view() {
        let mut cache = PageCache::with_budget(100 * MB);
        cache.insert(key(1, 0), page(10));
        cache.insert(key(1, 1), page(10));
        cache.insert(key(2, 0), page(10));

        assert_eq!(cache.remove_view(EntityId::from(1)).len(), 2);
        assert_eq!(cache.total_bytes(), 10 * MB);
        assert_eq!(cache.page_count(EntityId::from(1)), 0);
        assert!(cache.use_page(key(2, 0)).is_some());
    }
}
