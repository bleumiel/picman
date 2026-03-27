# PicMan

PicMan est une application desktop locale pour faire le menage dans des dossiers, detecter les doublons exacts sur tous types de fichiers ainsi que certaines copies reduites/recompressees sur image, et mettre en quarantaine les versions a supprimer.

## MVP actuel

- Scan recursif d'un ou plusieurs dossiers locaux
- Picker de dossier natif
- Detection des doublons exacts par hash `SHA-256` sur tous types de fichiers
- Detection conservative des copies reduites ou recompressees sur `JPEG` et `PNG`
- Miniatures directement dans les groupes de doublons
- Recommandation automatique du fichier a conserver
- Quarantaine non destructive des copies suggerees

## Stack

- Frontend: `React + TypeScript + Vite`
- Desktop shell: `Tauri v2`
- Backend local: `Rust`

## Lancement en developpement

```powershell
npm install
npm run tauri dev
```

## Build du frontend seul

```powershell
npm run build
```

## Build de l'application desktop

```powershell
npm install
npm run build:desktop
```

Le binaire a lancer ensuite est celui genere par **Tauri build**, pas `src-tauri\target\debug\picman.exe`.

Pour generer rapidement uniquement le `.exe` de release sans installeur :

```powershell
npm run build:desktop -- --no-bundle
```

En pratique, utilise l'un de ces artefacts :

- `src-tauri\target\release\picman.exe`
- ou l'installeur genere sous `src-tauri\target\release\bundle\`

`src-tauri\target\debug\picman.exe` est un binaire de developpement. Si tu le lances directement, il peut encore essayer d'ouvrir `http://localhost:1420`, ce qui provoque `ERR_CONNECTION_REFUSED` quand le serveur Vite n'est pas demarre.

## Notes

- Les copies reduites/recompressees sont detectees de facon conservative sur `JPEG` et `PNG`, sans tenter les recadrages.
- La quarantaine est creee dans le dossier scanne sous `.picman-quarantine`.
- Le plan de suivi projet est disponible dans `PLAN.md`.
