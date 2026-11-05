#!/bin/sh
# Удаление wayswitch-rs. Настройки и выученные слова удаляются только с --purge.
set -eu

systemctl --user disable --now wayswitch-rs.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/wayswitch-rs.service"
systemctl --user daemon-reload

sudo rm -f /usr/local/bin/wayswitch-rs /etc/udev/rules.d/60-wayswitch-rs.rules
sudo rm -rf /usr/local/share/doc/wayswitch-rs
sudo udevadm control --reload-rules

# Расширение GNOME общее с оригинальным WaySwitch, поэтому не трогаем его молча.
if [ -d "$HOME/.local/share/gnome-shell/extensions/wayswitch@siberia.ru" ]; then
    echo "Расширение GNOME оставлено: ~/.local/share/gnome-shell/extensions/wayswitch@siberia.ru"
fi

if [ "${1:-}" = "--purge" ]; then
    rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/wayswitch-rs" "${XDG_DATA_HOME:-$HOME/.local/share}/wayswitch-rs"
    echo "Настройки и выученные слова удалены."
else
    echo "Настройки и выученные слова оставлены. Удалить их: packaging/uninstall.sh --purge"
fi
