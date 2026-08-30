# Worker operations

Workers send a heartbeat every 30 seconds. The production timeout is 75 seconds
so one delayed heartbeat does not immediately evict a healthy worker.
