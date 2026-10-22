# Bringup dual 2026-10-21 (sender PC1 .100, receiver PC2 .59, adapter Controller)

sender=0.3.5 receiver=0.3.5 version_ok=True. NIC: PC1 I226-V Jumbo9014/EEE-off/IM-off; PC2 Realtek Jumbo9014/EEE-off/IM-off (set_nic.ps1).

| case | time | verdict | sent | complete | drop | lost | mm | avg | p99 | pps | cadence_ms | dst |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| T0 | 20261021-193245 | FAIL | 2400 | 2400 | 0 | 0 | 0 | 8.024 | 6.883 | 13440 | 4.971863192997082 | ff:ff:ff:ff:ff:ff |
| T1-checker | 20261021-194156 | FAIL | 1713 | 1713 | 0 | 0 | 0 | 13.377 | 6.273 | 1702 | 4.970366822429907 | ff:ff:ff:ff:ff:ff |
| T1-checker-rerun | 20261021-194723 | FAIL | 14400 | 14400 | 0 | 0 | 0 | 13.351 | 6.987 | 13549 | 5.022616292798111 | ff:ff:ff:ff:ff:ff |
| T1-counter | 20261021-194047 | FAIL | 14400 | 14400 | 0 | 0 | 0 | 13.408 | 6.356 | 13722 | 4.957483276616432 | ff:ff:ff:ff:ff:ff |
| T1-motion | 20261021-193650 | FAIL | 14400 | 14400 | 0 | 0 | 0 | 13.527 | 5.865 | 13350 | 5.09813528717272 | ff:ff:ff:ff:ff:ff |
| T1-noise | 20261021-195100 | FAIL | 14400 | 14400 | 0 | 0 | 0 | 4.99 | 5.823 | 13915 | 4.958090450725744 | ff:ff:ff:ff:ff:ff |
| T1-rings | 20261021-194946 | FAIL | 14400 | 14400 | 0 | 0 | 0 | 13.619 | 6.799 | 13216 | 5.150595256615043 | ff:ff:ff:ff:ff:ff |
| T2-soak-uni | 20261021-202819 | FAIL | 144000 | 144000 | 0 | 0 | 0 | 13.161 | 5.985 | 13925 | 4.948374132459253 | 00:E2:69:A9:F0:0A |
| T3-mtu1500 | 20261021-200337 | FAIL | 2400 | 2400 | 0 | 0 | 0 | 23.444 | 17.68 | 28262 | 14.506060650270946 | ff:ff:ff:ff:ff:ff |
| T4-p0 | 20261021-195951 | FAIL | 3600 | 3600 | 0 | 0 | 0 | 13.411 | 6.182 | 12964 | 5.039478216171158 | ff:ff:ff:ff:ff:ff |
| T4-p200 | 20261021-200224 | FAIL | 3600 | 3600 | 0 | 0 | 0 | 27.507 | 20.314 | 3567 | 19.06546635176438 | ff:ff:ff:ff:ff:ff |
| T4-p50 | 20261021-200024 | FAIL | 3600 | 3600 | 0 | 0 | 0 | 16.81 | 10.485 | 7829 | 8.53222814670742 | ff:ff:ff:ff:ff:ff |

Notes:
- delivery: all runs complete==sent, dropped=0, lost=0, mm=0; T0 offline reconcile 2400/2400 zero-mismatch; T2 online per-frame verify 144000 zero-mismatch.
- FAIL verdicts are terminal-ring avg artifact only (PLAN S12); cadence_ms column is the drain-free signal.
- T4 pacing follows packet-pacing physics; default pacing=0. T3 mtu1500 real-NIC is slow-not-lossy (69fps sender syscall bound).
- broadcast wedge at 70059 frames (~350s sender stall, zero errors); unicast clean through 144000. Default unicast until direct cable retest.
