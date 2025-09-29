# Concurrency Issues & Recommendations

## Known Issues

### 1. Hardware Stats Race Condition (src/tui/ui.rs:93-100)
**Location**: Hardware monitoring task in `run_app()`
**Issue**: Shared mutable state accessed from multiple threads without proper synchronization
**Current Code**:
```rust
let mut m = monitor.lock().await;
m.get_stats()
```
**Risk**: Medium - Could cause inconsistent readings
**Fix**: Use atomic operations or ensure proper lock ordering
**Priority**: P2

### 2. File Watcher Debouncing Missing (src/utils/file_watcher.rs)
**Issue**: No debouncing on file system events
**Impact**: Rapid file changes cause excessive reloads
**Recommended**: 300ms debounce window
**Priority**: P2

### 3. Lack of Cancellation Tokens
**Issue**: Long-running operations cannot be cancelled cleanly
**Impact**: Shutdown may not be graceful
**Recommended**: Add tokio::CancellationToken to all async operations
**Priority**: P3

### 4. Undefined Lock Ordering
**Issue**: No documented lock acquisition order
**Risk**: Potential deadlocks if locks taken in different orders
**Priority**: P2

## Recommendations

### Short Term (1-2 days)
1. Add cancellation tokens to all async operations
2. Implement file watcher debouncing (300ms)
3. Document lock ordering in ARCHITECTURE.md
4. Add lock timeout detection in debug builds

### Medium Term (1-2 weeks)
1. Audit all mutex usage for proper ordering
2. Add lock contention metrics
3. Implement circuit breaker for external API calls
4. Add request deduplication for duplicate LLM calls

### Long Term (1-2 months)
1. Consider actor model for better isolation
2. Implement task tracking with abort handles
3. Add distributed tracing for async operations
4. Performance profiling under high concurrency

## Testing Strategy

### Required Tests
1. Concurrent file watcher events
2. Multiple simultaneous model requests
3. Hardware monitor under stress
4. Lock timeout scenarios
5. Graceful shutdown with active operations

### Tools
- `tokio-console` for async task inspection
- `loom` for concurrency testing
- Custom stress tests with high parallelism

## Migration Path

### Phase 1: Safety (Week 1)
- Add cancellation tokens
- Document lock ordering
- Add debug assertions for lock violations

### Phase 2: Performance (Week 2-3)
- Implement debouncing
- Add metrics collection
- Profile under load

### Phase 3: Architecture (Month 2+)
- Consider actor model migration
- Implement supervision trees
- Add failure isolation

## Notes
- Most issues are low-probability in single-user CLI context
- Become critical if scaling to server/multi-user
- Current architecture is adequate for CLI use case
- Document for future reference and team onboarding