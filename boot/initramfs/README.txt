RustOS bootstrap initramfs (RIFS v1).

Он смонтирован read-only в /boot. Команды terminal:
  ls /boot
  cat /boot/README.txt

Записи в /home, /src и /build пока попадают в RAM overlay и исчезают после
перезагрузки. Persistent VaraniaFS подключится тем же VFS API через vfsd.
