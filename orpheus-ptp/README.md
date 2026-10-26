# orpheus-ptp

The PTP implementation (IEEE 1588-2019) under `orpheus-clock`. It is a copy
of [statime](https://github.com/pendulum-project/statime) 0.4.0, commit
`1fb674074ece5c492327ddd9982bc38dc0fa766e`, by Tweede Golf and contributors,
made so that Orpheus can change the port, the servo and the transport.

## Licence

statime is licensed under Apache-2.0 or MIT, at your option; Orpheus uses it
under MIT. Its notices are in [COPYRIGHT](COPYRIGHT),
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE) and still
apply to the code taken from statime.

Changes made in Orpheus since that commit are licensed under the GNU General
Public License, version 3 only ([LICENCE.md](../LICENCE.md)), and so is the
crate as a whole: `GPL-3.0-only AND MIT`.

## Updating from statime

The copy is a git subtree of the `statime/` directory of the statime
repository, squashed. To take in later statime changes:

```sh
git clone https://github.com/pendulum-project/statime /tmp/statime
git -C /tmp/statime subtree split --prefix=statime <tag> -b split
git subtree pull --prefix=orpheus-ptp --squash /tmp/statime split
```
