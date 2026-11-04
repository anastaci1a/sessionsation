#!/bin/sh
# nextest setup script: exports the test-only time compression to every test
# process (see src/timing.rs). An explicit SESSIONSATION_TEST_TIME_SCALE wins.
echo "SESSIONSATION_TEST_TIME_SCALE=${SESSIONSATION_TEST_TIME_SCALE:-10}" >> "$NEXTEST_ENV"
