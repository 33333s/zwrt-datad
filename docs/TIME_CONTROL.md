# Device time control

`time` in `/state` and the on-demand remote panel reports the current UTC epoch,
UTC and UTC+8 display strings, calibration configuration, operation receipts and
effective ownership/readback status. Status reads work without write privileges;
`write_supported` reports whether this process can set the system clock and
persist its configuration. `authenticated` is always false: unicast SNTP packet
validation is not cryptographic authentication of the time server.

Calibration is disabled by default. With no existing ownership journal, starting
datad in that default state does not change the clock, timezone, RTC or OEM time
provider. `boot_sync_enabled` defaults to true and is subordinate to the opt-in
calibration switch. Enabling calibration obtains a valid NTP sample before
changing the dedicated OEM NTP service or timezone. Boot synchronization retries
for at most 90 seconds; successful boot calibration is checked again after one
and three minutes, and small offsets do not cause another clock step.

## Typed actions

- `time.status`, with no parameters.
- `time.config.set`: optional `server`, `calibration_enabled`,
  `boot_sync_enabled`, `operation_tag`. At least one configuration field is
  required.
- `time.sync`: optional `server` and `operation_tag`.

Local writes require `confirmed: true` in the `/control` request envelope;
remote writes require the confirmed authenticated panel control protocol. A
server is a hostname, IPv4/IPv6 literal, or endpoint with a port from 1 to 65535;
URLs, credentials, paths, scope IDs, control characters and shell expressions
are refused. Operation tags are exactly 32 hexadecimal characters, generated
when omitted and echoed without changing case.

The immediate acknowledgement is `accepted`, not an assertion that the clock
has been calibrated. `configuration_saved` separately records persistence.
`last_operation` and the bounded `recent_operations` list expose the matching
terminal result. The most recent 16 operations are retained in memory; reuse of
a retained tag with changed parameters is refused, and matching retries do not
execute again. Only `phase: succeeded` and `verified: true` indicate verified
completion. A config-only operation does not create a `last_sync` event.

Phases are `idle`, `queued`, `applying`, `synchronized`, `restoring`, `failed` and
`conflict`. Verification is null while unknown or pending, false on failure and
true after readback. `last_sync.operation_tag` identifies its originating
operation. No backup content, shell output, raw OEM reply or credential is
included in these models.

## System ownership and recovery

System-clock changes use the Linux clock API; non-Linux hosts remain read-only.
The UTC+8 timezone representation is independently generated. Existing timezone
files, timezone UCI values and the initial enabled/running state of the fixed
`zte_topsw_ntp` service are recorded in a private journal. No other service,
including the general OEM event/time manager, is stopped or restarted.

Manual synchronization also works with calibration disabled. It applies one
clock/timezone change, records the owned timezone baseline and does not start
the guard or disable the OEM provider. Turning calibration off stops the guard
and restores only resources still matching datad's ownership journal; external
changes produce `restore_conflict` and are not overwritten. Partial operations
are journaled before mutation and can be recovered on restart. Configuration,
journal and backups are local private material, not remote API responses.

The guard uses a UTC sample plus boot elapsed time and boot identity, including
time spent suspended. Only the specifically observed positive UTC+8 step is
corrected; arbitrary clock changes by other owners are not undone. Clock and
timezone transitions, including rollback, hold an exclusive clock lease.
Date-sensitive operations hold a shared lease through their full consume,
reserve, execute and finish sequence. `clock_generation` advances when a
transition ends, allowing consumers to invalidate samples spanning that change.

SNTP requests have fresh nonces and connected UDP sockets. Replies must match
the request, be synchronized server responses with valid version/stratum and
nonzero plausible timestamps, and pass timing/root-distance bounds. DNS,
packet size, concurrency, query/retry budgets and cancellations are bounded;
post-2036 timestamps are decoded with an explicit era range. RTC support is
optional, and its write/readback outcome is reported separately.

The synthetic tests exercise clock and service ownership through isolated
fixtures. Hardware readback remains necessary for each device's init-script
semantics, filesystem persistence, RTC support and interactions with other time
providers. They must not be inferred from a successful mock.
