# Plan d'implementation - PicMan

## Objectif

PicMan doit aider a nettoyer des dossiers photo locaux en detectant les doublons, en choisissant la meilleure version a conserver, puis en proposant une suppression sure et reversible.

## Decisions retenues

- MVP en application desktop locale
- Stack retenue: TypeScript + Tauri
- Perimetre MVP: doublons exacts uniquement
- Formats MVP: JPEG, PNG et HEIC
- Suppression non destructive recommandee

## Etat actuel

- Le repo GitHub `https://github.com/bleumiel/picman.git` existe
- Le projet local est initialise et contient maintenant un premier socle Tauri + React
- Le moteur MVP actuel detecte les doublons exacts et peut mettre les copies en quarantaine
- Le scan photo est maintenant execute hors du thread UI avec une progression visible en temps reel pour eviter l'etat "Ne repond pas"
- L'UI propose maintenant un picker de dossier natif et des miniatures pour faciliter la revue des groupes
- Les groupes de doublons commencent a apparaitre pendant l'analyse et le scan evite maintenant de hasher les fichiers dont la taille est unique
- Le hash des candidats est maintenant parallélisé de façon modérée et une analyse en cours peut être annulée depuis l'interface
- Une meme analyse peut maintenant couvrir plusieurs dossiers a la fois, avec quarantaine dediee par racine et annulation plus reactive grace a des previews live limitees

## Phasage recommande

### Phase 1 - MVP fiable

- Synchroniser et maintenir le repo local avec GitHub
- Stabiliser le socle TypeScript + Tauri
- Scanner des dossiers locaux
- Indexer les photos et metadonnees principales
- Detecter les doublons exacts
- Calculer un score de qualite explicable
- Proposer automatiquement le fichier a garder
- Permettre une revue manuelle avant suppression
- Supprimer de facon reversible via une quarantaine

### Phase 2 - Evolutions

- Detection de doublons visuellement proches
- Prise en charge de formats supplementaires comme RAW
- Rescans incrementaux et optimisation grands volumes
- Regles de priorite configurables
- Journalisation/audit plus riche

## Backlog

1. `cadrer-mvp`
   - Perimetre produit fixe pour le MVP.

2. `synchroniser-repo-local`
   - Repo local initialise et relie au remote.

3. `preciser-architecture-tauri`
   - Architecture Tauri + React + Rust en place.

4. `initialiser-socle-projet`
   - Socle projet cree, build frontend et build desktop valides.

5. `modeliser-index-photo`
   - Modele de donnees pour les photos, groupes de doublons et decisions de conservation en place.

6. `implementer-scan-et-hachage`
   - Scan recursif et hachage SHA-256 implementes.

7. `detecter-doublons-exacts`
   - Groupement des doublons exacts operationnel.

8. `scorer-qualite-photo`
   - Score de qualite simple et explicable implemente pour la recommandation de conservation.

9. `construire-flux-revue-suppression`
   - UI de revue et quarantaine reversible disponibles.

10. `ajouter-tests-et-journalisation`
   - Tests Rust unitaires ajoutes; journalisation detaillee a renforcer dans une iteration suivante.

11. `polir-experience-de-scan`
   - Progression de scan visible, picker multi-dossiers, miniatures de groupe, pre-affichage des doublons pendant le scan, filtrage par taille, hash parallele modere, annulation reactive et quarantaine par racine en place; prochaine amelioration naturelle: progression de quarantaine, selection fine et parallelisation configurable.
