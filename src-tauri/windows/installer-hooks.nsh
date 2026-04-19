; Hooks NSIS appelés par le template Tauri à l'install et à la désinstall.
; Ajoute une exception Windows Firewall inbound pour le binaire installé,
; scopée aux 3 profils (Domain/Private/Public) — mDNS + libp2p TCP ont besoin
; d'être joignables sur LAN sans que l'utilisateur ait à changer son profil réseau.
;
; Sécurité : la règle est scope-binaire (pas port-wide), inbound uniquement,
; et toutes les données applicatives sont chiffrées E2E par la clé de salon.

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Configuration du pare-feu Windows pour LAN Chat…"
  ; Idempotent : on efface une éventuelle règle du même nom avant de (re)créer.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="LAN Chat"'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall add rule name="LAN Chat" description="Autorise LAN Chat à recevoir les connexions P2P sur le reseau local (mDNS + libp2p TCP). Les messages restent chiffres de bout en bout par la cle de salon." dir=in action=allow program="$INSTDIR\lan-chat.exe" enable=yes profile=domain,private,public'
  Pop $0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Suppression de la regle pare-feu LAN Chat…"
  nsExec::Exec 'netsh advfirewall firewall delete rule name="LAN Chat"'
  Pop $0

  ; Opt-in : proposer la suppression des donnees utilisateur (identite, historique,
  ; fichiers recus). En mode silencieux (/S), on conserve par defaut.
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "Supprimer aussi vos donnees LAN Chat ?$\r$\n$\r$\nInclut :$\r$\n  - identite P2P (ed25519)$\r$\n  - historique chiffre des conversations$\r$\n  - fichiers recus (dossier files)$\r$\n$\r$\nAction irreversible." \
    /SD IDNO \
    IDYES lanchat_wipe IDNO lanchat_keep

  lanchat_wipe:
    DetailPrint "Suppression des donnees LAN Chat dans $APPDATA\com.srfwb.lanchat"
    RMDir /r "$APPDATA\com.srfwb.lanchat"
    Goto lanchat_cleanup_end

  lanchat_keep:
    DetailPrint "Donnees LAN Chat conservees dans $APPDATA\com.srfwb.lanchat"

  lanchat_cleanup_end:
!macroend
