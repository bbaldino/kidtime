# Changelog

## [0.7.0](https://github.com/bbaldino/kidtime/compare/v0.6.0...v0.7.0) (2026-10-05)


### Features

* **agent:** enforce timers ([186b8fc](https://github.com/bbaldino/kidtime/commit/186b8fc508d0d45b3ad9d391f7cf0761f9f4d081))
* **dashboard:** start and cancel timers ([84ed33f](https://github.com/bbaldino/kidtime/commit/84ed33f283848ae65b1b412e3b66fd05d79cf266))
* **protocol:** add timers to the decision function ([d164964](https://github.com/bbaldino/kidtime/commit/d1649647224207406921db424bd23700b66801bd))
* **server:** start and cancel timers ([89dfaa3](https://github.com/bbaldino/kidtime/commit/89dfaa3757b0f02a29fcc7c38c7c1360f95f422e))


### Bug Fixes

* warn again for a second games timer, and word timer ends as such ([7eea16f](https://github.com/bbaldino/kidtime/commit/7eea16f394ad8c795041d11f7e961d88deb9f3d4))

## [0.6.0](https://github.com/bbaldino/kidtime/compare/v0.5.0...v0.6.0) (2026-10-05)


### Features

* send a message from the dashboard to a kid's computer ([c3dbaad](https://github.com/bbaldino/kidtime/commit/c3dbaadc3d4d73961ee7dd3272df72a2d6485e70))

## [0.5.0](https://github.com/bbaldino/kidtime/compare/v0.4.0...v0.5.0) (2026-10-04)


### Features

* **agent:** add the enforcement logic ([49353ff](https://github.com/bbaldino/kidtime/commit/49353ff2dec320ca2a0f6e563b9eecc9171215a9))
* **agent:** carry out enforcement on the system ([1c2b85e](https://github.com/bbaldino/kidtime/commit/1c2b85e9372dc56a6ccf1d27ae0febf01e33f6e8))
* **agent:** enforce the rules every 5 seconds and keep state across restarts ([614ba5b](https://github.com/bbaldino/kidtime/commit/614ba5be26e0322c6618250adcf74334f4e260b1))
* **agent:** find running apps' processes and sessions' lock state ([ff54d44](https://github.com/bbaldino/kidtime/commit/ff54d44f349a70035f941911c0d1411c669d57f0))
* **dashboard:** add the Enforce switch and show what enforcement did ([8c4eb43](https://github.com/bbaldino/kidtime/commit/8c4eb43e587a081a39c40c4f3feda1a711f9e6e5))
* **protocol:** add the report response snapshot and report extras ([dcd43d1](https://github.com/bbaldino/kidtime/commit/dcd43d1ed96fd2d7b56833175437484a0688d71b))
* **server:** answer reports with per-account snapshots and add the Enforce switch ([8bb7ef2](https://github.com/bbaldino/kidtime/commit/8bb7ef248c7f64520a361bb8b3ae674876700b2b))


### Bug Fixes

* **agent:** escalate to SIGKILL however far apart close calls are ([1d338ee](https://github.com/bbaldino/kidtime/commit/1d338eef77c9793939fe655e4b15bc83026fe416))
* **agent:** harden enforcement against crashes, hangs and the child ([86e1161](https://github.com/bbaldino/kidtime/commit/86e1161e1675e81a4fc78fba9d6fd819af70b627))
* **agent:** keep blocks across reboots and stops, and report failed releases ([5b9d8c2](https://github.com/bbaldino/kidtime/commit/5b9d8c2349d32f79e3157821ebcfe4b8bad1c5b5))
* **agent:** make enforcement commands safe against odd input and passwordless accounts ([7fcb009](https://github.com/bbaldino/kidtime/commit/7fcb009fb39159a1cb2de7d7327f16ecf6ed3ecf))
* **agent:** never block on the stream socket, and report unlocks only when seen ([bba975d](https://github.com/bbaldino/kidtime/commit/bba975d6e672e583e0e85f338eb8211b07488266))
* carry the week of rules in the snapshot so blocks hold past midnight ([8e3efeb](https://github.com/bbaldino/kidtime/commit/8e3efeb3661f8c6398769aef4ce086fd4b8894b9))
* **dashboard:** disable the Enforce switch while its change is saving ([e41f206](https://github.com/bbaldino/kidtime/commit/e41f206ec4e9d5cc139790f915d7721388063586))
* **deploy:** make banner setup failures take the fallback, and clean up on uninstall ([5fcb41f](https://github.com/bbaldino/kidtime/commit/5fcb41ffd83f792178a60b4478b7bde3420ce9df))
* make uninstall keep the lock record unless everything was released ([457a4fd](https://github.com/bbaldino/kidtime/commit/457a4fdde1ae86414e1ab73ff78eb367ee3d156c))

## [0.4.0](https://github.com/bbaldino/kidtime/compare/v0.3.0...v0.4.0) (2026-10-03)


### Features

* copy a kid's week of rules to another kid ([140e288](https://github.com/bbaldino/kidtime/commit/140e288349f180cfd316e769e78118492c6605e2))

## [0.3.0](https://github.com/bbaldino/kidtime/compare/v0.2.1...v0.3.0) (2026-10-03)


### Features

* add an Ignored category and count uncategorised apps as games ([953edcd](https://github.com/bbaldino/kidtime/commit/953edcd34b9c2f9e123fc88bd365ec6c0c6000dc))

## [0.2.1](https://github.com/bbaldino/kidtime/compare/v0.2.0...v0.2.1) (2026-10-02)


### Bug Fixes

* **dashboard:** name scripts and styles with a content stamp ([ed6f5fe](https://github.com/bbaldino/kidtime/commit/ed6f5fe29828a53055a14f7705f30c035de8d260))

## [0.2.0](https://github.com/bbaldino/kidtime/compare/v0.1.1...v0.2.0) (2026-10-02)


### Features

* **dashboard:** add rules, blackouts and app categories ([3359966](https://github.com/bbaldino/kidtime/commit/33599668116995df5beadb84462d622fcec05acf))
* **server:** add rule types and the decision function ([8a9403b](https://github.com/bbaldino/kidtime/commit/8a9403b9f6678b4533327341f5f6b048b07e0fb3))
* **server:** add the rules API and put the dashboard behind the login check ([59c5d38](https://github.com/bbaldino/kidtime/commit/59c5d38f010a30e72aff6274b555ffb0db1facf7))
* **server:** catalogue apps and count time per category ([81dc4ba](https://github.com/bbaldino/kidtime/commit/81dc4ba2348242793ddf3f71306cb4356b6cf3f9))
* **server:** log what the rules would have done ([bdb8751](https://github.com/bbaldino/kidtime/commit/bdb8751687580c73f1d47742a5d2c388130b011a))
* **server:** store schedules, budgets and blackouts ([bd6f99f](https://github.com/bbaldino/kidtime/commit/bd6f99f60ec6bf441a808e1a20ac55cdcb1364a2))
* **server:** verify the reverse proxy's login token ([ad472cb](https://github.com/bbaldino/kidtime/commit/ad472cb1ff2c00deca314e2bfbce15adfd39ef9f))


### Bug Fixes

* **dashboard:** don't throw when a decision has no next change ([2958776](https://github.com/bbaldino/kidtime/commit/2958776e413f541de8d90af07e54aa9cc61516c4))
* **dashboard:** show failed saves and loads, and let new apps be marked reviewed ([461a494](https://github.com/bbaldino/kidtime/commit/461a4949505998ee7909234b95d785e8b465544e))
* **server:** tighten the would-have log, the key fetch and what reports can store ([b300932](https://github.com/bbaldino/kidtime/commit/b300932883bdc2408641be9243c70ac900d86843))

## [0.1.1](https://github.com/bbaldino/kidtime/compare/v0.1.0...v0.1.1) (2026-10-02)


### Bug Fixes

* **dashboard:** send cookies with the manifest request ([fe7c70c](https://github.com/bbaldino/kidtime/commit/fe7c70c7a86637d56d8bea436463bb63f7d8c05f))
