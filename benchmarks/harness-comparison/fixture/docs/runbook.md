# Operations runbook

The production gateway listens on port 7412 and its admin endpoint on 7413.
Page the ledger owner if invoice p95 exceeds 225 ms for ten minutes.
Page the identity owner if authentication errors exceed 2 percent.

Retry policy: seven attempts, starting at 125 ms and capped at 2000 ms.
Cache TTL is 900 seconds for successful invoice lookups and 30 seconds for misses.
