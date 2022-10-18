//! Allocation tracking exposed via internal telemetry.

// TODO: We don't currently register the root allocation group which means we're missing all the allocations that happen
// outside of the various component tasks. Additionally, we are likely missing span propagation for spawned tasks that
// occur under component tasks. Likely, not for sure... but likely.
//
// TODO: Another big chunk of memory use at low load will be thread stacks, especially with how big/deep futures
// generators are. Thread stacks can be large: 8MB by default on Linux. This means that if we use a lot of blocking
// tasks, we're at least making calls for 8MB chunks for each thread, which is still virtual memory so it's not actually
// physically wired in all at once... but it's a large potential lever on memory usage, again, with how big/deep futures
// generators are.
//
// This is made harder to track because, for example, on Linux, `pthread_create` is using `mmap -- a syscall -- to get a
// private anonymous region for a thread's stack, so we can't even capture that allocation in our user-mode global
// allocator.
//
// TODO: Maybe we should track VSZ/RSS overall for the process so that we can at least emit it alongside the allocation
// metrics to have more of a full picture.. as you could intuit from the above TODOs, the numbers may still diverge
// quite a bit but they should all be correlated/directional enough to tell the full story.
//
// TODO: Could we take a reference to the span that we want to attach the allocation group token to, and then visit all
// of the fields to automatically extract the relevant metric tags? We could then also attach the token to the span for
// the caller, so that they never even needed to bother doing that. This would be cleaner than having to generate the
// vector of tags by hand, which is obviously a "do it once and then never change it" thing but would look a lot cleaner
// in this proposed method.
//
// TODO: We should explore a design where we essentially bring in `tracking-allocator` directly and tweak it such that
// we collapse a majority of the various thread locals, and lean more on what we already do, specifically related to the
// span stack.
//
// Essentially, since we always need to coordinate with the span stack to enter in and out, could we use the span stack
// to actually store the (de)alloc counts and amounts and then push them as an aggregated event when the current group
// is popped off the stack? We'd still have to enter the thread local in `alloc`/`dealloc` so it might be a wash, and
// there's also the question of how we handle tasks that infrquently yield (aka would exit the span) or don't yield at
// all... then we're back in "tracked a bunch of events but never sent them" territory... but... there might be
// something we could do here *shrug*

mod allocator;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use arr_macro::arr;

use self::allocator::{enable_allocation_tracing, without_allocation_tracing, Tracer};

pub(crate) use self::allocator::{
    AllocationGroupId, AllocationGroupToken, AllocationLayer, GroupedTraceableAllocator,
};

static GROUP_MEM_METRICS: [AtomicU64; 1024] = arr![AtomicU64::new(0); 1024];

pub type Allocator<A> = GroupedTraceableAllocator<A, LocalProducerTracer>;

pub const fn get_grouped_tracing_allocator<A>(allocator: A) -> Allocator<A> {
    GroupedTraceableAllocator::new(allocator, LocalProducerTracer)
}

pub struct LocalProducerTracer;

impl Tracer for LocalProducerTracer {
    fn trace_allocation(&self, wrapped_size: usize, group_id: AllocationGroupId) {
        GROUP_MEM_METRICS[group_id.as_usize().get()]
            .fetch_add(wrapped_size as u64, Ordering::SeqCst);
    }

    fn trace_deallocation(&self, wrapped_size: usize, source_group_id: AllocationGroupId) {
        GROUP_MEM_METRICS[source_group_id.as_usize().get()]
            .fetch_sub(wrapped_size as u64, Ordering::SeqCst);
    }
}

/// Initializes allocation tracing.
pub fn init_allocation_tracing() {
    let alloc_processor = thread::Builder::new().name("vector-alloc-processor".to_string());
    alloc_processor
        .spawn(move || {
            without_allocation_tracing(move || loop {
                for idx in 0..GROUP_MEM_METRICS.len() {
                    let atomic_ref = GROUP_MEM_METRICS.get(idx).unwrap();
                    let mem_used = atomic_ref.load(Ordering::Relaxed);
                    if mem_used == 0 {
                        continue;
                    }

                    info!(
                        message = "group memory usage",
                        group_id = idx,
                        current_memory_allocated_in_bytes = mem_used
                    );
                }
                thread::sleep(Duration::from_millis(5000));
            })
        })
        .unwrap();
    enable_allocation_tracing();
}

/// Acquires an allocation group ID.
///
/// This creates an allocation group which allows callers to enter/exit the allocation group context, associating all
/// (de)allocations within the context with that group. An allocation group ID must be "attached" to
/// a [`tracing::Span`] to achieve this" we utilize the logical invariants provided by spans --
/// entering, exiting, and how spans exist as a stack -- in order to handle keeping the "current
/// allocation group" accurate across all threads.
///
/// # Tags
///
/// The provided `tags` are used for the metrics that get registered and attached to the allocation group. No tags from
/// the traditional `metrics`/`tracing` are collected, as the metrics are updated directly rather than emitted via the
/// traditional `metrics` macros, so the given tags should match the span fields that would traditionally be set for a
/// given span in order to ensure that they match.
pub fn acquire_allocation_group_id(_tags: Vec<(String, String)>) -> AllocationGroupToken {
    // TODO: register the allocation group token with its tags via `Collector`: we can't do it via `Registrations`
    // because that gets checked lazily/periodically, and we need to be able to associate a group ID with its tags
    // immediately so that we don't misassociate events
    AllocationGroupToken::register().expect("failed to register allocation group token")
}
