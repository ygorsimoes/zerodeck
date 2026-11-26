use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, recovering the guard if a previous holder panicked.
///
/// Every value `ZeroDeck` keeps behind a [`Mutex`] stays consistent between statements, so a
/// poisoned lock carries no torn state and is safe to keep using.
pub fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::lock_unpoisoned;

    #[test]
    fn lock_unpoisoned_recovers_after_panic() {
        let shared = Arc::new(Mutex::new(41));
        let poisoner = Arc::clone(&shared);
        let joined = std::thread::spawn(move || {
            let mut guard = poisoner.lock().expect("first lock");
            *guard += 1;
            panic!("poison the mutex while holding it");
        })
        .join();
        assert!(joined.is_err());
        assert!(shared.is_poisoned());

        assert_eq!(*lock_unpoisoned(&shared), 42);
    }
}
