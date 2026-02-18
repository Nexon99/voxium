# Guide pratique de l’API utilisateur non officielle Discord

> Basé sur la doc **Discord Userdoccers** (docs.discord.food), qui documente le côté utilisateur de l’API, utilisé par le client officiel et le Dev Portal, mais non supporté par Discord. [github](https://github.com/discord-userdoccers/discord-userdoccers)

---

## ⚠️ Avertissements & contexte

L’API décrite ici est :

- **Non officielle** : maintenue par la communauté “Discord Userdoccers”, non affiliée à Discord. [docs.discord](https://docs.discord.food)
- **Axée comptes utilisateur** : elle couvre l’API telle qu’utilisée par des **tokens user / bearer**, pas les bots. [github](https://github.com/discord-userdoccers/discord-userdoccers)
- **Basée sur du reverse / observation** : certaines infos peuvent être incomplètes ou susceptibles de changer sans préavis. [docs.discord](https://docs.discord.food)

Automatiser des comptes utilisateur est contre les **Terms of Service** de Discord ; un usage abusif ou massif peut mener à des locks ou bans de comptes. [docs.discord](https://docs.discord.food)

---

## Architecture générale côté “user”

L’écosystème API utilisateur non officiel, tel que documenté par Userdoccers, se découpe en plusieurs briques : [github](https://github.com/discord-userdoccers/discord-userdoccers)

- **HTTP API “user-side”** : mêmes domaines que l’API officielle (`https://discord.com/api/vX`), mais utilisée avec des **tokens utilisateur ou bearer**.
- **Gateway temps réel** : WebSocket principal pour la présence, messages en temps réel, etc. (non détaillé ici, la doc Userdoccers couvre les events).
- **Remote Auth Gateway** : Gateway WebSocket spéciale pour la connexion par QR code (Remote Authentication). [docs.discord](https://docs.discord.food/remote-authentication/overview)
- **Ressources / objets** : pages `/resources/*` décrivant les structures d’objets user-side (user settings, connected accounts, applications…). [docs.discord](https://docs.discord.food/resources/connected-accounts)

L’org GitHub `discord-userdoccers` regroupe la doc et les projets liés au reverse de la plateforme (protos de user settings, etc.). [github](https://github.com/discord-userdoccers)

---

## Authentification non officielle (vue d’ensemble)

### Tokens utilisateur & sessions

Sur un compte utilisateur, Discord utilise un **login classique e‑mail/mot de passe**, qui renvoie un **token utilisateur** stocké côté client et utilisé ensuite sur l’API. [docs.discord](https://docs.discord.food/authentication)

Points importants décrits dans la page “Authentication” de docs.discord.food : [docs.discord](https://docs.discord.food/authentication)

- **Fingerprints** : identifiants utilisés pour coller les expérimentations A/B au long du flow auth, envoyés via l’entête `X-Fingerprint` et dans certains JSON params tant que l’utilisateur n’est pas loggé. [docs.discord](https://docs.discord.food/authentication)
- **Sessions** : Discord suit des “auth sessions” liées aux tokens, avec des infos comme OS, plateforme et localisation approx., et peut flag des sessions suspectes (reset password, locks, etc.). [docs.discord](https://docs.discord.food/authentication)

L’API non officielle **décrit ces concepts** (fingerprint, auth session object, events Gateway de changement de session), mais ne fournit pas de flow “clé en main” pour se log via mot de passe : l’idée est d’expliquer comment le client officiel fonctionne, pas de fournir un SDK de login. [docs.discord](https://docs.discord.food)

### Remote Authentication (QR code)

La **Remote Authentication** est un autre mécanisme central documenté par Userdoccers : elle permet de transférer un token d’un device mobile loggé vers un device “desktop” via un QR code. [docs.discord](https://docs.discord.food/remote-authentication/mobile)

Principe général : [discord.neko](https://discord.neko.wtf/remote_auth/)

- Le “desktop client” ouvre un WebSocket vers `wss://remote-auth-gateway.discord.gg/?v=2` (ou version indiquée). [discord.neko](https://discord.neko.wtf/remote_auth/)
- Il fait un **échange de clés** (actuellement RSA 2048 bits côté desktop, clé publique envoyée via un opcode `init`). [discord.neko](https://discord.neko.wtf/remote_auth/)
- Le gateway renvoie un `nonce` chiffré, que le client déchiffre et renvoie en clair via `nonce_proof`.
- Le gateway envoie ensuite un opcode `pending_remote_init` contenant un **fingerprint**, qui sert à générer l’URL du QR : `https://discord.com/ra/{fingerprint}`. [discord.neko](https://discord.neko.wtf/remote_auth/)
- Le mobile scanne le QR, extrait le fingerprint, puis crée une **session Remote Auth** via l’endpoint `POST /users/@me/remote-auth` avec `{ fingerprint }`. [docs.discord](https://docs.discord.food/remote-authentication/mobile)
- Le mobile peut ensuite **accepter ou refuser** via `POST /users/@me/remote-auth/finish` ou un endpoint d’annulation correspondant. [docs.discord](https://docs.discord.food/remote-authentication/mobile)
- Si accepté, le desktop reçoit, via le même WebSocket Remote Auth, un blob chiffré contenant le **token utilisateur**, qu’il déchiffre avec sa clé privée. [docs.discord](https://docs.discord.food/remote-authentication/overview)

Cette mécanique est intégralement décrite (opcodes, champs, endpoints Remote Auth, format de l’URL QR) dans : [docs.discord](https://docs.discord.food/remote-authentication/overview)

- `Remote Authentication - Overview` (docs.discord.food)
- `Remote Authentication (Mobile)` (docs.discord.food)
- Des pages de reverse supplémentaires (comme discord-undocumented / remote_auth). [discord.neko](https://discord.neko.wtf/remote_auth/)

---

## Ressources / objets clés côté utilisateur

Les pages `/resources/*` de docs.discord.food décrivent la forme des objets utilisés par l’API utilisateur (payloads HTTP et Gateway). [docs.discord](https://docs.discord.food)

### User Settings

La page **User Settings** documente tous les champs de configuration d’un compte Discord (généraux + par serveur). [docs.discord](https://docs.discord.food/resources/user-settings)

Exemples de champs (non exhaustif) : [docs.discord](https://docs.discord.food/resources/user-settings)

- `afk_timeout` : durée (en secondes) avant que le client ne mette l’utilisateur en AFK.
- `developer_mode` : toggle pour afficher les IDs, etc.
- `explicit_content_filter` : niveau de filtrage de contenu explicite.
- `custom_status` : objet décrivant le statut personnalisé global.
- `activity_restricted_guild_ids` / `activity_joining_restricted_guild_ids` : guilds où la présence / le join d’activité sont restreints.

La doc précise la **structure de l’objet User Settings**, ainsi que les notions de **User Guild Settings** pour les préférences par serveur (notifications, DMs, etc.). [docs.discord](https://docs.discord.food/resources/user-settings)

### Connected Accounts (Connections)

La page **Connected Accounts** décrit l’objet `Connection`, qui représente un compte externe lié (Steam, Reddit, etc.). [docs.discord](https://docs.discord.food/resources/connected-accounts)

Champs importants : [docs.discord](https://docs.discord.food/resources/connected-accounts)

- `id` : identifiant du compte externe.
- `type` : type de connexion (`steam`, `twitch`, `reddit`, etc.).
- `name` : pseudo sur le service externe.
- `verified` : booléen indiquant si la connexion est vérifiée.
- `metadata` : objet de métadonnées spécifiques au service (par ex. `total_karma`, `created_at` pour Reddit).
- `visibility` / `metadata_visibility` : contrôlent la visibilité publique de la connexion et de ses métadonnées. [docs.discord](https://docs.discord.food/resources/connected-accounts)
- `friend_sync`, `show_activity`, `two_way_link`, `revoked`, `integrations`, éventuellement `access_token` (non présent dans tous les contextes). [docs.discord](https://docs.discord.food/resources/connected-accounts)

Ces structures servent à reproduire l’onglet “Connexions” et tout ce qui tourne autour des Linked Roles/Connections dans ton client custom. [support.discord](https://support.discord.com/hc/en-us/articles/8063233404823-Connections-Linked-Roles-Community-Members)

### Applications

La page **Applications** (Applications Resource) détaille comment sont représentées les applications Discord (jeux, intégrations, apps) côté API. [docs.discord](https://docs.discord.food/resources/application)

On y trouve notamment : [docs.discord](https://docs.discord.food/resources/application)

- Les champs de base : `id`, `name`, `description`, `icon`, `cover_image`, `flags`, etc.
- Des infos sur les exécutables, les SKUs, les intégrations de type “game overlay”, etc.

C’est utile si tu veux reconstituer un onglet “Bibliothèque” ou afficher les détails d’une application dans ton client perso. [docs.discord](https://docs.discord.food/resources/application)

---

## Patterns communs de l’API utilisateur

Même si beaucoup d’endpoints user-side ne sont pas listés dans la doc officielle, leurs **patterns** restent proches : [github](https://github.com/discord-userdoccers/discord-userdoccers)

- **Base URL** : `https://discord.com/api/v{version}` (actuellement v10 pour beaucoup d’endpoints côté officiel), mais certains appels utilisateur non documentés peuvent encore pointer vers d’autres versions.
- **Authentification** :
  - tokens utilisateur / bearer passés via `Authorization` (format exact dépendant du type de token).
  - possibilité d’informations supplémentaires via entêtes comme `X-Fingerprint` selon l’étape du flow auth. [docs.discord](https://docs.discord.food/authentication)
- **Snowflakes** : identifiants 64 bits encodés en string, réutilisés partout (users, guilds, channels, messages, etc.). [docs.discord](https://docs.discord.food)
- **Pagination & limites** : beaucoup d’endpoints respectent les patterns `limit`, `before`, `after` que tu connais déjà sur l’API bot (non détaillés ici mais cohérents). [docs.discord](https://docs.discord.food)

La doc Userdoccers couvre aussi : [github](https://github.com/discord-userdoccers/discord-userdoccers)

- Des topics transverses (auth, expériments, erreurs, etc.).
- Les structures Gateway (events, ready, presence, etc.).
- Des projets annexes comme `discord-protos` pour reverse les protobufs des user settings. [github](https://github.com/discord-userdoccers)

---

## Exemple de “stack” logique pour un client custom

Voici comment utiliser l’API non officielle **sans détailler de endpoints sensibles**, en restant dans une optique de client perso :

1. **Obtention d’un token utilisateur**
   - Via Remote Auth (QR), en implémentant le rôle *desktop* (WebSocket Remote Auth Gateway) et en utilisant un client mobile officiel pour transférer le token. [docs.discord](https://docs.discord.food/remote-authentication/overview)
   - Ou via un token déjà fourni manuellement (par exemple pour des tests sur ton propre compte).

2. **Initialisation de session**
   - Ouvre une session HTTP en ajoutant systématiquement l’en‑tête `Authorization` avec ton token.
   - Eventuellement, gère les fingerprints / entêtes additionnels si tu reproduis aussi le flow de login / handoff décrit dans la page “Authentication”. [docs.discord](https://docs.discord.food/authentication)

3. **Chargement des infos de base**
   - Utilise les ressources Userdoccers pour reconstruire :
     - Le profil utilisateur (basé sur l’objet “User” documenté officiellement + compléments user-side). [discord](https://discord.com/developers/docs/resources/user)
     - Les **User Settings** (pour afficher/modifier les préférences). [docs.discord](https://docs.discord.food/resources/user-settings)
     - Les **Connected Accounts** (pour l’onglet connexions). [support.discord](https://support.discord.com/hc/en-us/articles/8063233404823-Connections-Linked-Roles-Community-Members)
     - Les guilds, canaux, etc. via les structures standard.

4. **UI & logique métier**
   - Mappe les structures `/resources/*` sur tes écrans :
     - Formulaires et toggles pour les user settings. [docs.discord](https://docs.discord.food/resources/user-settings)
     - Listes et détails pour les connected accounts. [docs.discord](https://docs.discord.food/resources/connected-accounts)
     - Cartes / tuiles pour les applications. [docs.discord](https://docs.discord.food/resources/application)

5. **Sécurité & respect des règles**
   - Ne réimplémente pas un vrai “login par mot de passe” dans un environnement peu sécurisé ; préfère Remote Auth ou l’usage de tokens déjà gérés par le client officiel. [docs.discord](https://docs.discord.food/remote-authentication/overview)
   - Ne log pas les tokens, ne les envoie pas à des services tiers, chiffre‑les au repos.

---

## Où lire la doc non officielle

Pour aller plus loin ou vérifier les détails :

- **Site principal** :
  - Introduction à l’API utilisateur non officielle, scope, avertissements : `https://docs.discord.food`. [docs.discord](https://docs.discord.food)

- **Repo GitHub principal** :
  - `discord-userdoccers/discord-userdoccers` : sources MDX du site, très pratique pour chercher des occurrences d’endpoints / champs. [github](https://github.com/discord-userdoccers/discord-userdoccers)

- **Remote Auth** :
  - Overview + sections Desktop/Mobile sur docs.discord.food. [docs.discord](https://docs.discord.food/remote-authentication/mobile)
  - Pages de reverse plus verbeuses sur d’autres sites (ex. `discord-undocumented` pour les opcodes détaillés). [discord.neko](https://discord.neko.wtf/remote_auth/)

- **Ressources importantes** :
  - User Settings : `docs.discord.food/resources/user-settings`. [docs.discord](https://docs.discord.food/resources/user-settings)
  - Connected Accounts : `docs.discord.food/resources/connected-accounts`. [docs.discord](https://docs.discord.food/resources/connected-accounts)
  - Applications : `docs.discord.food/resources/application`. [docs.discord](https://docs.discord.food/resources/application)

---

Si tu veux, tu peux préciser dans quoi tu codes ton client (C++/ImGui, TS/Electron, Rust, etc.) et quelles parties de l’API user tu veux cibler en priorité, et ce Markdown peut être complété par une section “plan d’implémentation” très concrète adaptée à ta stack.
