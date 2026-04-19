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
!macroend
