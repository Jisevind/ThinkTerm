import SwiftUI

/// The desktop's sidebar, as the TUI lays it out -- the host, its Spaces,
/// their projects, their threads -- drawn the system's way: large rows
/// with line icons, section titles, a rounded fill on the thread on show.
/// On a phone held upright it is the drawer the screen slides off; on a
/// wide screen it stays at the left. Everything is named as the desktop
/// names it.
struct SidebarPanel: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    var hostName: String
    var connected: Bool
    /// Something was picked: a drawer closes, a column stays.
    var done: () -> Void
    var onEditHost: () -> Void
    var onSettings: () -> Void
    var onOverview: () -> Void
    /// The overview is up: its row is lit.
    var overviewOn = false
    /// Someone sees it: a drawer shut under the screen makes no rows, and
    /// keeps its folds and search for when it opens.
    var live = true
    /// Folded nodes; every Space but the one on show starts folded.
    @State private var folded: Set<String>?
    @State private var searching = false
    @State private var search = ThreadSearch()
    /// Where the system puts bars along the top and bottom (`barRoom`):
    /// on some screens further in than the safe area's edges.
    @State private var headerRoom = EdgeInsets()
    @State private var footerRoom = EdgeInsets()

    private var spaces: [TreeSpace] { model.tree?.spaces ?? [] }

    private var connectionColor: Color {
        if model.reconnecting { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    private static let backdrop = Color(white: 0.07)
    /// The header's row and the footer's, their padding included.
    private static let headerHeight: CGFloat = 60
    private static let footerHeight: CGFloat = 64

    var body: some View {
        Group {
            if live { list } else { Color.clear }
        }
        .foregroundColor(.white)
        .background(Self.backdrop.ignoresSafeArea())
        .onChange(of: spaces.isEmpty, initial: true) { _, empty in
            // Once the tree is in: the panel is made before it arrives.
            if folded == nil, !empty {
                folded = Set(spaces.filter { !$0.current }.map { "space:" + $0.id })
            }
        }
    }

    /// The list runs the panel's full height, under the host at the top
    /// and the buttons at the bottom, fading as it goes.
    private var list: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                if searching {
                    ThreadSearchResults(search: search, spaces: spaces) { space, thread in
                        unfold("space:" + space.id)
                        model.openThread(thread.id, space: space.id)
                        searching = false
                        search.query = ""
                        done()
                    }
                } else {
                    tree
                }
            }
            .padding(.horizontal, 10)
            .padding(.bottom, 12)
        }
        .scrollDismissesKeyboard(.immediately)
        .floatingBar(edge: .top, fading: Self.backdrop, scrim: true) {
            VStack(spacing: 0) {
                header
                if searching { searchField }
            }
        }
        .floatingBar(edge: .bottom, fading: Self.backdrop) {
            if !searching { footer }
        }
        .barRoom(.top, height: Self.headerHeight, $headerRoom)
        .barRoom(.bottom, height: Self.footerHeight, $footerRoom)
    }

    // MARK: the top

    /// The host -- the tree's root and the connection's menu -- and the
    /// search button.
    private var header: some View {
        HStack(spacing: 10) {
            Menu {
                Button(connected ? tr("disconnect") : tr("reconnect")) {
                    if connected { model.disconnect() } else { model.connect() }
                }
                if model.host != nil {
                    Button(tr("edithost")) { onEditHost() }
                }
            } label: {
                HStack(spacing: 8) {
                    Text(hostName)
                        .font(.title2.weight(.bold))
                        .lineLimit(1)
                    Circle().fill(connectionColor).frame(width: 8, height: 8)
                }
            }
            .buttonStyle(.plain)
            Spacer(minLength: 0)
            Button {
                withAnimation(.easeOut(duration: 0.2)) {
                    searching.toggle()
                    if !searching { search.query = "" }
                }
                // The field takes the keyboard as it comes up, not each
                // time the panel makes it again.
                search.focusNext = searching
            } label: {
                Image(systemName: searching ? "xmark" : "magnifyingglass")
                    .font(.body.weight(.medium))
                    .frame(width: 44, height: 44)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .glassCircle()
            .accessibilityLabel(tr(searching ? "cancel" : "thread.find"))
        }
        .padding(.leading, max(20, headerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6 + max(0, headerRoom.top))
        .padding(.bottom, 10)
    }

    private var searchField: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass").foregroundColor(.secondary)
            SearchField(search: search, prompt: tr("thread.find"))
        }
        .padding(.horizontal, 14)
        .frame(height: 40)
        .glassCapsule()
        .padding(.horizontal, 14)
        .padding(.bottom, 8)
        .transition(.move(edge: .top).combined(with: .opacity))
    }

    // MARK: the bottom

    /// A new thread, where the thumb is, and the settings, floating over
    /// the end of the list. Searching puts them away.
    private var footer: some View {
        HStack(spacing: 12) {
            Button {
                model.sideClick("new-thread")
                done()
            } label: {
                Label(tr("thread.new"), systemImage: "square.and.pencil")
                    .font(.body.weight(.semibold))
                    .lineLimit(1)
                    .minimumScaleFactor(0.8)
                    .padding(.horizontal, 18)
                    .frame(height: 50)
                    .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .glassCapsule(tint: .accentColor)
            Spacer(minLength: 0)
            Button { done(); onSettings() } label: {
                Image(systemName: "gearshape")
                    .font(.title3)
                    .frame(width: 50, height: 50)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .glassCircle()
            .accessibilityLabel(tr("settings"))
        }
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        .padding(.leading, max(14, footerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6)
        // Below the safe area's edge where the system's bars go there.
        .padding(.bottom, 8 + footerRoom.bottom)
    }

    // MARK: the tree

    @ViewBuilder
    private var tree: some View {
        // The screen shuts the drawer itself, and waits for it.
        SidebarRow(icon: "square.grid.2x2", title: tr("overview"), selected: overviewOn) { onOverview() }

        // The Space on show's, as the desktop pins them; they leave their
        // projects' lists.
        let pinned = spaces.filter(\.current).flatMap { space in
            space.projects.flatMap { project in
                project.threads.filter(\.pinned).map { (space: space, project: project, thread: $0) }
            }
        }
        if !pinned.isEmpty {
            sectionTitle(tr("pinned"))
            ForEach(pinned, id: \.thread.id) { entry in
                threadRow(entry.thread, space: entry.space, depth: 0, subtitle: entry.project.name)
            }
        }

        HStack {
            sectionTitle(tr("workspaces"))
            Spacer()
            Button {
                model.sideClick("new-project")
                model.askName()
            } label: {
                Image(systemName: "plus")
                    .font(.subheadline.weight(.medium))
                    .foregroundColor(.secondary)
                    .frame(width: 36, height: 36)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(tr("project.new"))
        }
        .contentShape(Rectangle())
        // A lone Space has no row (below): its menu -- a new Space, a new
        // name -- is held here.
        .contextMenu {
            if spaces.count == 1 { AppMenuItems(model: model, kind: "space", id: "") }
        }
        // One Space needs no row of its own: its projects stand at the top.
        if spaces.count == 1, let space = spaces.first {
            ForEach(space.projects) { project in projectRows(space, project, depth: 0) }
        } else {
            ForEach(spaces) { space in spaceRows(space) }
        }
        tailRows
        if let error = model.sidebar?.new_project_error {
            Text(error).font(.footnote).foregroundColor(.red).padding(.horizontal, 12).padding(.vertical, 8)
        }
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(text)
            .font(.subheadline)
            .foregroundColor(.secondary)
            .padding(.horizontal, 12)
            .padding(.top, 16)
            .padding(.bottom, 2)
    }

    private func isFolded(_ key: String) -> Bool { folded?.contains(key) ?? false }

    private func toggle(_ key: String) {
        var set = folded ?? []
        if set.contains(key) { set.remove(key) } else { set.insert(key) }
        withAnimation(.easeOut(duration: 0.18)) { folded = set }
    }

    private func unfold(_ key: String) {
        guard isFolded(key) else { return }
        toggle(key)
    }

    @ViewBuilder
    private func spaceRows(_ space: TreeSpace) -> some View {
        let key = "space:" + space.id
        SidebarRow(
            icon: "square.stack.3d.up",
            title: space.name,
            bold: space.current,
            chevron: space.projects.isEmpty ? nil : !isFolded(key)
        ) {
            // Another Space is gone to, open; the one on show folds.
            if space.current {
                toggle(key)
            } else {
                model.setSpace(space.id)
                unfold(key)
            }
        }
        .contextMenu { AppMenuItems(model: model, kind: "space", id: "") }
        if !isFolded(key) {
            ForEach(space.projects) { project in projectRows(space, project, depth: 1) }
        }
    }

    @ViewBuilder
    private func projectRows(_ space: TreeSpace, _ project: TreeProject, depth: Int) -> some View {
        let key = "project:" + project.id
        // Pinned threads have their own rows, the Space on show's only.
        let threads = space.current ? project.threads.filter { !$0.pinned } : project.threads
        SidebarRow(
            icon: "folder",
            title: project.name,
            depth: depth,
            chevron: threads.isEmpty ? nil : !isFolded(key),
            trailing: AnyView(
                Button {
                    model.setSpace(space.id)
                    model.sideClick("new-thread", id: project.id)
                    done()
                } label: {
                    Image(systemName: "plus")
                        .font(.subheadline)
                        .foregroundColor(.secondary)
                        .frame(width: 32, height: 32)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(tr("thread.new"))
            )
        ) { toggle(key) }
        .contextMenu { AppMenuItems(model: model, kind: "project", id: project.id) }
        if !isFolded(key) {
            ForEach(threads) { thread in
                threadRow(thread, space: space, depth: depth + 1)
            }
        }
    }

    private func threadRow(_ thread: ThreadRow, space: TreeSpace, depth: Int, subtitle: String? = nil) -> some View {
        SidebarRow(
            mark: AnyView(ThreadMark(status: thread.status, dot: thread.dot, size: 9)),
            title: thread.name,
            subtitle: subtitle,
            depth: depth,
            bold: thread.unread,
            dim: !thread.live,
            selected: thread.selected
        ) {
            model.openThread(thread.id, space: space.id)
            done()
        }
        .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
    }

    /// What the desktop lists after the tree: archived projects of the
    /// Space on show, and windows no thread claims.
    @ViewBuilder
    private var tailRows: some View {
        let rows = model.sidebar?.rows ?? []
        let tail = Array(rows.drop(while: { row in
            if case .archived = row { return false }
            if case .others = row { return false }
            return true
        }))
        ForEach(tail) { row in
            switch row {
            case .archived(_, let open, let label):
                // The label carries the count.
                SidebarRow(icon: "archivebox", title: label, dim: true, chevron: open) {
                    model.sideClick("toggle-archived", flag: !open)
                }
            case .project(let id, let name, _, _, true):
                SidebarRow(icon: "folder", title: name, depth: 1, dim: true) {}
                    .contextMenu { AppMenuItems(model: model, kind: "archived-project", id: id) }
            case .others:
                sectionTitle(tr("otherwindows"))
            case .window(let id, let title, let selected):
                SidebarRow(icon: "macwindow", title: title.isEmpty ? tr("window.n", id) : title, selected: selected) {
                    model.sideClick("window", id: String(id))
                    done()
                }
            default:
                EmptyView()
            }
        }
    }
}

// MARK: finding a thread

/// What the sidebar's search looks for, apart from the panel: only the
/// field and the results read it, so typing redraws them and not the
/// panel round them (its bars, their glass).
@Observable final class ThreadSearch {
    var query = ""
    /// The field takes the keyboard as it comes up next.
    @ObservationIgnored var focusNext = false
}

/// Every thread whose name holds the query, wherever it is, with the
/// project it is in.
private struct ThreadSearchResults: View {
    let search: ThreadSearch
    let spaces: [TreeSpace]
    let pick: (TreeSpace, ThreadRow) -> Void

    var body: some View {
        let needle = search.query.trimmingCharacters(in: .whitespaces)
        let found = spaces.flatMap { space in
            space.projects.flatMap { project in
                project.threads
                    .filter { needle.isEmpty || $0.name.localizedCaseInsensitiveContains(needle) }
                    .map { (space: space, project: project, thread: $0) }
            }
        }
        if found.isEmpty {
            Text(tr("thread.find.none"))
                .font(.subheadline)
                .foregroundColor(.secondary)
                .padding(12)
        }
        ForEach(found, id: \.thread.id) { entry in
            SidebarRow(
                mark: AnyView(ThreadMark(status: entry.thread.status, dot: entry.thread.dot, size: 9)),
                title: entry.thread.name,
                subtitle: entry.project.name,
                selected: entry.thread.selected
            ) {
                pick(entry.space, entry.thread)
            }
        }
    }
}

/// The search's field as UIKit has it, which tells when a word is still
/// being put together -- pinyin before its characters are picked: the
/// query waits for the characters, as no thread is named in the letters.
private struct SearchField: UIViewRepresentable {
    let search: ThreadSearch
    let prompt: String

    func makeCoordinator() -> Coordinator { Coordinator(search: search) }

    func makeUIView(context: Context) -> UITextField {
        let field = UITextField()
        // What it held when the panel last made it (a drawer shut and
        // opened again).
        field.text = search.query
        field.attributedPlaceholder = NSAttributedString(string: prompt, attributes: [.foregroundColor: UIColor.placeholderText])
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.maximumContentSizeCategory = .extraExtraLarge
        field.textColor = .white
        field.autocapitalizationType = .none
        field.autocorrectionType = .no
        field.spellCheckingType = .no
        field.returnKeyType = .search
        field.delegate = context.coordinator
        field.addTarget(context.coordinator, action: #selector(Coordinator.edited(_:)), for: .editingChanged)
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        DispatchQueue.main.async {
            if search.focusNext {
                search.focusNext = false
                field.becomeFirstResponder()
            }
        }
        return field
    }

    func updateUIView(_ field: UITextField, context: Context) {
        context.coordinator.search = search
    }

    final class Coordinator: NSObject, UITextFieldDelegate {
        var search: ThreadSearch

        init(search: ThreadSearch) { self.search = search }

        @objc func edited(_ field: UITextField) {
            guard field.markedTextRange == nil else { return }
            search.query = field.text ?? ""
        }

        func textFieldShouldReturn(_ field: UITextField) -> Bool {
            field.resignFirstResponder()
            return true
        }
    }
}

/// One row of the sidebar: an icon or a status mark, the title, an
/// optional fold chevron and trailing button; a rounded fill when it is
/// the one on show. Its text follows the system's size.
private struct SidebarRow: View {
    var icon: String?
    var mark: AnyView?
    var title: String
    var subtitle: String?
    var depth = 0
    var bold = false
    var dim = false
    var selected = false
    /// Unfolded (true), folded (false), or nothing to fold (nil).
    var chevron: Bool?
    var trailing: AnyView?
    var action: () -> Void

    init(
        icon: String? = nil, mark: AnyView? = nil, title: String, subtitle: String? = nil,
        depth: Int = 0, bold: Bool = false, dim: Bool = false, selected: Bool = false,
        chevron: Bool? = nil, trailing: AnyView? = nil, action: @escaping () -> Void
    ) {
        self.icon = icon
        self.mark = mark
        self.title = title
        self.subtitle = subtitle
        self.depth = depth
        self.bold = bold
        self.dim = dim
        self.selected = selected
        self.chevron = chevron
        self.trailing = trailing
        self.action = action
    }

    var body: some View {
        HStack(spacing: 12) {
            Group {
                if let mark {
                    mark
                } else if let icon {
                    Image(systemName: icon).font(.body).foregroundColor(.secondary)
                }
            }
            .frame(width: 24)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.body.weight(bold ? .semibold : .regular))
                    .foregroundColor(dim ? .secondary : .primary)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle).font(.footnote).foregroundColor(.secondary).lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            if let trailing { trailing }
            if let chevron {
                Image(systemName: "chevron.right")
                    .font(.footnote.weight(.semibold))
                    .foregroundColor(.secondary)
                    .rotationEffect(.degrees(chevron ? 90 : 0))
            }
        }
        .padding(.leading, 12 + CGFloat(depth) * 18)
        .padding(.trailing, 10)
        .frame(minHeight: 44)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(selected ? Color.white.opacity(0.13) : Color.clear)
        )
        .contentShape(Rectangle())
        .onTapGesture(perform: action)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }
}

extension View {
    /// The system's Liquid Glass round a button, from iOS 26; a material
    /// before it.
    @ViewBuilder
    func glassCircle() -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(.regular.interactive(), in: .circle)
        } else {
            background(.ultraThinMaterial, in: Circle())
        }
    }

    /// The same, in a capsule (a field, a row of buttons); tinted, the one
    /// button that leads.
    @ViewBuilder
    func glassCapsule(tint: Color? = nil) -> some View {
        if #available(iOS 26.0, *) {
            if let tint {
                glassEffect(.regular.tint(tint).interactive(), in: .capsule)
            } else {
                glassEffect(.regular, in: .capsule)
            }
        } else if let tint {
            background(tint, in: Capsule())
        } else {
            background(.ultraThinMaterial, in: Capsule())
        }
    }

    /// A bar at one end of a scrolling list that the list runs under,
    /// fading there rather than cut: the system's soft scroll edge from
    /// iOS 26, a fade into `color` before it. A bar whose words must stay
    /// readable over the list (`scrim`) keeps the fade on every version.
    func floatingBar<Bar: View>(
        edge: VerticalEdge, fading color: Color, scrim: Bool = false, @ViewBuilder _ bar: () -> Bar
    ) -> some View {
        modifier(FloatingBar(edge: edge, color: color, scrim: scrim, bar: bar()))
    }

    /// Reads where the system puts a bar `height` tall along `edge` of the
    /// screen: how far that is from the safe area's edges (`BarRoom`), for
    /// a bar there to add to its margins -- at the sides, a bar that
    /// reaches that side of the screen.
    func barRoom(_ edge: VerticalEdge, height: CGFloat, _ room: Binding<EdgeInsets>) -> some View {
        background(BarRoom(edge: edge, height: height, room: room))
    }

    /// Glass shapes side by side, drawn together as the system draws its
    /// own groups of buttons, from iOS 26.
    @ViewBuilder
    func glassGroup(spacing: CGFloat) -> some View {
        if #available(iOS 26.0, *) {
            GlassEffectContainer(spacing: spacing) { self }
        } else {
            self
        }
    }

    /// A tab's capsule in the glass of the buttons beside it, the tab on
    /// show lit with the bar's ink.
    @ViewBuilder
    func glassPill(lit: Bool, ink: Color) -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(lit ? .regular.tint(ink.opacity(0.2)).interactive() : .regular.interactive(), in: .capsule)
        } else {
            background(ink.opacity(lit ? 0.18 : 0.06), in: Capsule())
        }
    }
}

private struct FloatingBar<Bar: View>: ViewModifier {
    let edge: VerticalEdge
    let color: Color
    let scrim: Bool
    let bar: Bar

    private var edges: Edge.Set { edge == .top ? .top : .bottom }

    @ViewBuilder
    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content
                .safeAreaBar(edge: edge) { system }
                .scrollEdgeEffectStyle(.soft, for: edges)
        } else {
            content.safeAreaInset(edge: edge, spacing: 0) { faded }
        }
    }

    @ViewBuilder
    private var system: some View {
        if scrim { faded } else { bar }
    }

    /// The bar over `color`, solid at the screen's edge and clear at the
    /// bar's inner one.
    private var faded: some View {
        bar.background(
            LinearGradient(
                stops: [
                    .init(color: color, location: 0),
                    .init(color: color, location: 0.7),
                    .init(color: color.opacity(0), location: 1),
                ],
                startPoint: edge == .top ? .top : .bottom,
                endPoint: edge == .top ? .bottom : .top
            )
            .ignoresSafeArea(edges: edges)
        )
    }
}

/// Where the system lays a bar along the top or bottom of the screen,
/// which is not always the safe area's edge: an unfolded Duo, its status
/// bar at the side, has no safe area at the top, and a bar there belongs
/// below the rounded corners; at the bottom of a phone a bar sits partly
/// over the home indicator, as the system's tab bar does. iOS 27.1 says
/// where (`UIView.LayoutRegion.bar`); from iOS 26 the tab bar's place
/// stands in at the bottom. Measured on the window, from its safe area's
/// edges (a bottom bar's room below it is negative): the view backing a
/// SwiftUI one can be read mid-layout, somewhere it never shows, and is
/// given no safe area of its own.
private struct BarRoom: UIViewRepresentable {
    let edge: VerticalEdge
    let height: CGFloat
    @Binding var room: EdgeInsets

    func makeUIView(context: Context) -> Reader {
        let reader = Reader()
        reader.isUserInteractionEnabled = false
        return reader
    }

    func updateUIView(_ reader: Reader, context: Context) {
        let room = $room
        reader.found = { if room.wrappedValue != $0 { room.wrappedValue = $0 } }
        reader.edge = edge
        reader.height = height
        reader.read()
    }

    /// Reads the room again whenever the screen may have changed under
    /// it, and tells only a change, after the pass.
    final class Reader: UIView {
        var edge = VerticalEdge.top
        var height: CGFloat = 0
        var found: ((EdgeInsets) -> Void)?
        private var told: EdgeInsets?

        override func didMoveToWindow() {
            super.didMoveToWindow()
            read()
        }

        override func layoutSubviews() {
            super.layoutSubviews()
            read()
        }

        func read() {
            guard let window, height > 0 else { return }
            let rtl = window.effectiveUserInterfaceLayoutDirection == .rightToLeft
            let insets = window.safeAreaInsets
            let safe = NSDirectionalEdgeInsets(
                top: insets.top,
                leading: rtl ? insets.right : insets.left,
                bottom: insets.bottom,
                trailing: rtl ? insets.left : insets.right
            )
            var room = EdgeInsets()
            if #available(iOS 27.1, *) {
                let bar = window.directionalEdgeInsets(
                    for: .bar(onEdge: edge == .top ? NSDirectionalRectEdge.top : .bottom, extent: height)
                )
                room = EdgeInsets(
                    top: edge == .top ? bar.top - safe.top : 0,
                    leading: bar.leading - safe.leading,
                    bottom: edge == .bottom ? bar.bottom - safe.bottom : 0,
                    trailing: bar.trailing - safe.trailing
                )
            } else if #available(iOS 26.0, *), edge == .bottom, safe.bottom >= 20 {
                // The tab bar's middle is 52 points above the edge of a
                // phone with a home indicator.
                room.bottom = min(0, 52 - height / 2 - safe.bottom)
            }
            guard room != told else { return }
            told = room
            let found = self.found
            DispatchQueue.main.async { found?(room) }
        }
    }
}
