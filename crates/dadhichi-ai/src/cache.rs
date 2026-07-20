//! Client-side completion caching.
//!
//! Two layers of caching cut cost and latency in Dadhichi:
//!
//! - **Provider-side prompt caching** — marking a [`Message`](crate::Message)
//!   with [`cached()`](crate::Message::cached) inserts a cache breakpoint so the
//!   provider (Anthropic today) reuses the KV-cache for a stable prefix.
//! - **Client-side response caching** — this module. A [`CachingModel`] wraps
//!   any [`LanguageModel`] and short-circuits *identical* requests with the
//!   stored completion, never hitting the network at all. This is what makes an
//!   agent re-running the same sub-query effectively free.

use crate::provider::{LanguageModel, ModelCapabilities, ProviderResult};
use crate::types::{Completion, CompletionRequest, StreamChunk};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A hash-keyed store of completions with hit/miss accounting.
///
/// Keying is by a stable hash of the request (model, messages, sampling
/// params), so only *byte-identical* requests hit. Bounded by `capacity`; once
/// full it stops admitting new entries rather than evicting — simple and
/// predictable, and a good fit for an editing session's bursty repeats.
#[derive(Debug)]
pub struct CompletionCache {
    entries: Mutex<HashMap<u64, Completion>>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CompletionCache {
    /// Create a cache holding at most `capacity` completions.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// A stable key for `request`.
    fn key(request: &CompletionRequest) -> u64 {
        let mut hasher = DefaultHasher::new();
        request.model.hash(&mut hasher);
        for m in &request.messages {
            (m.role as u8).hash(&mut hasher);
            m.content.hash(&mut hasher);
        }
        // Fold sampling params in via their bit patterns so different
        // temperatures don't collide.
        request.params.temperature.to_bits().hash(&mut hasher);
        request.params.top_p.to_bits().hash(&mut hasher);
        request.params.max_tokens.hash(&mut hasher);
        request.params.stop.hash(&mut hasher);
        hasher.finish()
    }

    /// Look up a cached completion, recording a hit or miss.
    pub fn get(&self, request: &CompletionRequest) -> Option<Completion> {
        let found = self
            .entries
            .lock()
            .expect("cache poisoned")
            .get(&Self::key(request))
            .cloned();
        if found.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Store `completion` for `request` (no-op once at capacity).
    pub fn put(&self, request: &CompletionRequest, completion: Completion) {
        let mut entries = self.entries.lock().expect("cache poisoned");
        if entries.len() >= self.capacity && !entries.contains_key(&Self::key(request)) {
            return;
        }
        entries.insert(Self::key(request), completion);
    }

    /// Number of cache hits so far.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Number of cache misses so far.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

/// Wraps a [`LanguageModel`] with a client-side [`CompletionCache`].
///
/// `complete` serves identical repeat requests from cache; `stream` always goes
/// to the inner model (streamed responses are not cached).
#[derive(Debug)]
pub struct CachingModel<M> {
    inner: M,
    cache: CompletionCache,
}

impl<M> CachingModel<M> {
    /// Wrap `inner`, caching up to `capacity` completions.
    pub fn new(inner: M, capacity: usize) -> Self {
        Self {
            inner,
            cache: CompletionCache::new(capacity),
        }
    }

    /// Access the underlying cache (for hit/miss stats).
    pub fn cache(&self) -> &CompletionCache {
        &self.cache
    }
}

#[async_trait]
impl<M: LanguageModel> LanguageModel for CachingModel<M> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
        if let Some(hit) = self.cache.get(&request) {
            return Ok(hit);
        }
        let completion = self.inner.complete(request.clone()).await?;
        self.cache.put(&request, completion.clone());
        Ok(completion)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> ProviderResult<BoxStream<'static, ProviderResult<StreamChunk>>> {
        self.inner.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{LanguageModel, MockProvider};
    use crate::types::{Message, Role};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Counts how many times the inner model is actually invoked.
    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<AtomicU64>,
        inner: MockProvider,
    }

    #[async_trait]
    impl LanguageModel for CountingProvider {
        fn id(&self) -> &str {
            "counting"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.complete(request).await
        }
    }

    #[tokio::test]
    async fn identical_requests_hit_cache() {
        let calls = Arc::new(AtomicU64::new(0));
        let model = CachingModel::new(
            CountingProvider {
                calls: calls.clone(),
                inner: MockProvider::default(),
            },
            16,
        );

        let req = CompletionRequest::new("m").message(Message::user("hello"));
        let a = model.complete(req.clone()).await.unwrap();
        let b = model.complete(req.clone()).await.unwrap();

        assert_eq!(a.content, b.content);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "inner model called once");
        assert_eq!(model.cache().hits(), 1);
        assert_eq!(model.cache().misses(), 1);
    }

    #[tokio::test]
    async fn different_requests_miss() {
        let cache = CompletionCache::new(8);
        let a = CompletionRequest::new("m").message(Message::user("one"));
        let b = CompletionRequest::new("m").message(Message::user("two"));
        assert!(cache.get(&a).is_none());
        cache.put(
            &a,
            Completion {
                content: "x".into(),
                model: "m".into(),
                usage: Default::default(),
                tool_calls: Vec::new(),
            },
        );
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
    }

    #[test]
    fn capacity_is_respected() {
        let cache = CompletionCache::new(1);
        let a = CompletionRequest::new("m").message(Message::user("a"));
        let b = CompletionRequest::new("m").message(Message::user("b"));
        let c = Completion {
            content: "c".into(),
            model: "m".into(),
            usage: Default::default(),
            tool_calls: Vec::new(),
        };
        cache.put(&a, c.clone());
        cache.put(&b, c.clone());
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none(), "second entry rejected at capacity");
    }

    #[test]
    fn role_is_part_of_the_key() {
        // Guard against a key that ignores role: same content, different role.
        let cache = CompletionCache::new(8);
        let mut a = CompletionRequest::new("m");
        a.messages.push(Message::user("hi"));
        let mut b = CompletionRequest::new("m");
        b.messages.push(Message::assistant("hi"));
        cache.put(
            &a,
            Completion {
                content: "x".into(),
                model: "m".into(),
                usage: Default::default(),
                tool_calls: Vec::new(),
            },
        );
        assert!(cache.get(&b).is_none());
    }
}
