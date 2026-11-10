# Heartbeat natif — spécification de présence (RigInfinity)
# Créé au pas 1 de D-N051 (automaton) — la doctrine heartbeat devient native.
# Source : automaton/src/heartbeat/tasks.ts (tâches A'Space, gel e025526)
# + principes-fusion-runtime.md §3 (R1, R2).
# Le daemon qui implémente cette spécification est l'infinity-daemon (pas 3).
# Présence = daemon + événements, pas cron aveugle.

## Règle
Le heartbeat est une checklist AVEC décision, pas un sondage aveugle.
Il ne parle que s'il y a une action ou un résultat utilisable (SuperAgent).
Entre les événements : hibernation (Infinity), coût nul.
Après un crash : reprise au checkpoint (Pi Durable), pas redémarrage à zéro.
Exactly-once par requestId. Un déploiement interrompu est rapporté,
jamais répété.

## Tâches (transcrites de tasks.ts)
- **heartbeat_ping** : le daemon signale qu'il est vivant (nom, adresse, version, sandboxId). Horodaté, consigné.
- **check_credits** : vérifie le solde de crédits d'inférence. Mode souverain : le daemon ne meurt jamais à 0 crédit USDC — il bascule sur le compute local (freellmapi/auto, 127.0.0.1:31415) et le failover multi-modèles.
- **check_usdc_balance** : vérifie le solde USDC (Conway). En mode souverain, aucun top-up automatique : la mort par marché est inapplicable (la mort = perte de confiance de S0).
- **check_social_inbox** : sonde la boîte sociale (GitHub, Feishu) avec curseur et backoff. Ne réveille que s'il y a un item de travail.
- **aspace_ceo_3os_sync** : synchronise les 3 étages holoniques (Tech OS 13, Life OS 43, Areas OS 522 = 578 Holons). Active le mode souverain (`aspace_sovereign_mode=enabled`). Consigne `aspace_last_3os_sync`.
- **aspace_beth_homeostasis** : lit l'état Beth (`GREEN | ORANGE | RED | HALT_LD03 | HALT_LD04`). HALT = arrêt de l'étage concerné, jamais de travail forcé. Consigne `aspace_beth_state`.

## Garde-fous
- Sanctuaire nocturne 00h00–05h00 (Beth) : pas de travail lourd, pas de décision irréversible.
- Un battement manqué (> 2 h sans pulsation, 4 battements de 30 min) = anomalie, routée à Yaz (watchlist) et à Rick (L00 dead-man par Worker, D-N049 D11).
- Le vérificateur L00 est lui-même vérifié (clause dead-man, W5 appliqué à L00).
