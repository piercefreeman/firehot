use std::sync::{Arc, Condvar, Mutex};

/// A generic data structure for asynchronously resolving values with blocking capability.
/// This allows a thread to wait for a value to be resolved, even if the resolution happens
/// before or after the thread starts waiting.
#[derive(Clone)]
pub struct AsyncResolve<T: Clone> {
    /// The optional resolved value
    value: Arc<Mutex<Option<T>>>,
    /// Condition variable for notification when value is resolved
    condition: Arc<(Mutex<bool>, Condvar)>,
}

impl<T: Clone> AsyncResolve<T> {
    /// Creates a new unresolved AsyncResolve instance
    pub fn new() -> Self {
        Self {
            value: Arc::new(Mutex::new(None)),
            condition: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    /// Sets the resolved value and notifies all waiters
    pub fn resolve(&self, value: T) {
        // Set the value
        {
            let mut value_guard = self.value.lock().unwrap();
            *value_guard = Some(value);
        }

        // Notify all waiters
        let (mutex, condvar) = &*self.condition;
        let mut completed = mutex.lock().unwrap();
        *completed = true;
        condvar.notify_all();
    }

    /// Blocks until the value is resolved or returns immediately if already resolved
    pub fn wait(&self) -> Result<T, String> {
        // First check if value is already resolved to avoid unnecessary locking
        {
            let value_guard = self.value.lock()
                .map_err(|e| format!("Failed to lock value mutex: {:?}", e))?;
            
            if let Some(value) = &*value_guard {
                return Ok(value.clone());
            }
        }
        
        // Otherwise, wait for resolution
        let (mutex, condvar) = &*self.condition;
        let completed = mutex.lock()
            .map_err(|e| format!("Failed to lock completion mutex: {:?}", e))?;
        
        // If not completed, wait for the signal
        if !*completed {
            let _completed = condvar.wait(completed)
                .map_err(|e| format!("Failed to wait on condvar: {:?}", e))?;
        }
        
        // Now that we've been signaled, the value should be available
        let value_guard = self.value.lock()
            .map_err(|e| format!("Failed to lock value mutex after wait: {:?}", e))?;
        
        match &*value_guard {
            Some(value) => Ok(value.clone()),
            None => Err("Value should be resolved but is not available".to_string()),
        }
    }

    /// Non-blocking check if value is resolved
    pub fn is_resolved(&self) -> bool {
        let value_guard = self.value.lock().unwrap();
        value_guard.is_some()
    }

    /// Non-blocking attempt to get the resolved value
    pub fn get(&self) -> Option<T> {
        let value_guard = self.value.lock().unwrap();
        value_guard.clone()
    }
}

impl<T: Clone> Default for AsyncResolve<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_resolve_before_wait() {
        let resolver = AsyncResolve::new();
        resolver.resolve(42);
        
        let result = resolver.wait().unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_resolve_after_wait() {
        let resolver = AsyncResolve::new();
        
        let resolver_clone = resolver.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            resolver_clone.resolve(42);
        });
        
        let result = resolver.wait().unwrap();
        assert_eq!(result, 42);
        
        handle.join().unwrap();
    }

    #[test]
    fn test_is_resolved() {
        let resolver = AsyncResolve::<i32>::new();
        assert!(!resolver.is_resolved());
        
        resolver.resolve(42);
        assert!(resolver.is_resolved());
    }

    #[test]
    fn test_get() {
        let resolver = AsyncResolve::<String>::new();
        assert_eq!(resolver.get(), None);
        
        resolver.resolve("hello".to_string());
        assert_eq!(resolver.get(), Some("hello".to_string()));
    }
} 