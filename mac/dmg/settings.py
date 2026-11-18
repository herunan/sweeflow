# dmgbuild settings for Sweeflow.dmg (release.sh): the window that opens with the disk image, with
# its background (background.svg, rendered by make-background.sh) and the two icons placed on it.
# release.sh passes: -D app=<Sweeflow.app> -D here=<this folder>.
import os.path

app = defines["app"]
here = defines["here"]

format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}
# The disk's own icon: Sweeflow's.
icon = os.path.join(app, "Contents/Resources/AppIcon.icns")

# 640 x 460 points, the background's size; its @2x copy is used on Retina screens.
background = os.path.join(here, "background.png")
window_rect = ((200, 140), (640, 460))
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"
show_icon_preview = False
include_icon_view_settings = True
arrange_by = None
icon_size = 112
text_size = 13
label_pos = "bottom"
icon_locations = {
    "Sweeflow.app": (170, 150),
    "Applications": (470, 150),
}
