# PicMan

PicMan est une application desktop locale pour faire le menage dans des dossiers photo, detecter les doublons exacts ainsi que certaines copies reduites/recompressees, et mettre en quarantaine les versions a supprimer.

## MVP actuel

- Scan recursif d'un dossier local
- Picker de dossier natif
- Prise en charge des formats `JPEG`, `PNG` et `HEIC`
- Detection des doublons exacts par hash `SHA-256`
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

## Build

```powershell
npm run build
```

## Notes

- Les copies reduites/recompressees sont detectees de facon conservative sur `JPEG` et `PNG`, sans tenter les recadrages.
- La quarantaine est creee dans le dossier scanne sous `.picman-quarantine`.
- Le plan de suivi projet est disponible dans `PLAN.md`.
