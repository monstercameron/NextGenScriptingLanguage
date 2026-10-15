import Quickshell
import Quickshell.Io
import QtQuick
import QtQuick.Controls
import QtQuick.Window

// Generic renderer only: application data, formatting, workflow decisions and
// action handling stay in .program applets. This file owns QML object lifetime
// and transports a typed button signal back through an explicit event cursor.
ShellRoot {
    id: root

    property string sessionId: ""
    // Rust substitutes a JSON-escaped unique target into its private runtime
    // copy. The checked-in source never reads an ungranted environment value.
    property string ipcTarget: "__EXECUTE_RENDERER_IPC_TARGET__"
    property int generation: 0
    property int revision: 0
    property var widgets: ({})
    property var eventQueue: []
    property int nextEventCursor: 1
    property bool eventOverflow: false
    property int maxQueuedEvents: 256
    property int maxWidgets: 256
    property int maxFrameBytes: 1048576
    property int negotiatedVersion: 0
    property int protocolMinimum: 1
    property int protocolMaximum: 1
    property var activeRequest: null

    ListModel {
        id: widgetModel
        dynamicRoles: true
    }

    IpcHandler {
        target: root.ipcTarget

        // Quickshell IPC's typed CLI surface accepts scalar values; one string
        // carries the versioned JSON frame and keeps schema validation in Rust.
        function request(frame: string): string {
            return root.dispatch(frame);
        }
    }

    Window {
        id: surface
        visible: true
        title: "Execute generic renderer"
        width: 520
        height: Math.max(90, Math.min(620, widgetModel.count * 54 + 30))
        color: "#1e1e2e"

        Column {
            anchors.fill: parent
            anchors.margins: 12
            spacing: 8

            Repeater {
                model: widgetModel

                delegate: Loader {
                    id: widgetLoader
                    required property string widgetKey
                    required property string appletId
                    required property string widgetId
                    required property string componentName
                    required property string text
                    required property bool enabled
                    required property int sourceRevision

                    width: surface.width - 24
                    height: 42
                    sourceComponent: componentName === "button" ? buttonComponent : textComponent

                    onLoaded: {
                        item.text = widgetLoader.text;
                        if (widgetLoader.componentName === "button")
                            item.enabled = widgetLoader.enabled;
                    }

                    Connections {
                        target: widgetLoader.item
                        enabled: widgetLoader.componentName === "button"

                        function onActivated() {
                            root.queueSignal(
                                widgetLoader.widgetKey,
                                widgetLoader.appletId,
                                widgetLoader.widgetId,
                                "clicked",
                                widgetLoader.sourceRevision
                            );
                        }
                    }
                }
            }
        }
    }

    component TextComponent: Rectangle {
        property string text: ""
        radius: 6
        color: "#313244"
        Text {
            anchors.fill: parent
            anchors.leftMargin: 12
            verticalAlignment: Text.AlignVCenter
            color: "#cdd6f4"
            text: parent.text
        }
    }

    component ButtonComponent: Button {
        property string text: ""
        signal activated()
        onClicked: activated()
    }

    Component {
        id: textComponent
        TextComponent {}
    }

    Component {
        id: buttonComponent
        ButtonComponent {}
    }

    function dispatch(frameText: string): string {
        let request;
        if (frameText.length > maxFrameBytes)
            return encodeError("frame_too_large", "request frame exceeds negotiated limit");
        try {
            request = JSON.parse(frameText);
        } catch (error) {
            activeRequest = null;
            return encodeError("invalid_json", "request frame is not valid JSON");
        }
        activeRequest = request;

        if (request === null || Array.isArray(request) || typeof request !== "object"
                || request.body === null || typeof request.body !== "object"
                || typeof request.body.kind !== "string")
            return encodeError("invalid_request", "request frame must contain a command body");
        if (request.protocol !== "execute.renderer")
            return encodeError("protocol_mismatch", "unsupported renderer protocol");

        if (request.body && request.body.kind === "negotiate")
            return negotiate(request);

        if (negotiatedVersion === 0 || request.version !== negotiatedVersion)
            return encodeError("version_mismatch", "renderer protocol was not negotiated");
        if (request.session_id !== sessionId || request.generation !== generation)
            return encodeError("stale_session", "request belongs to another renderer generation");
        if (!Number.isInteger(request.revision) || request.revision < 0)
            return encodeError("invalid_revision", "request revision must be a nonnegative integer");
        if ((request.body.kind === "poll_events" || request.body.kind === "ack_events")
                && request.revision !== revision)
            return encodeError("stale_revision", "event operation uses a stale renderer revision");

        switch (request.body.kind) {
        case "instantiate":
            return instantiate(request);
        case "patch":
            return patch(request);
        case "destroy":
            return destroy(request);
        case "poll_events":
            return pollEvents(request);
        case "ack_events":
            return ackEvents(request);
        default:
            return encodeError("unknown_command", "unsupported renderer command");
        }
    }

    function negotiate(request): string {
        const offered = request.body.supported;
        if (!offered || !Number.isInteger(offered.minimum) || !Number.isInteger(offered.maximum)
                || offered.minimum < 1 || offered.maximum < offered.minimum)
            return encodeError("invalid_version_range", "invalid renderer version range");

        const selected = Math.min(offered.maximum, protocolMaximum);
        if (selected < Math.max(offered.minimum, protocolMinimum))
            return encodeError("no_common_version", "renderer protocol ranges do not overlap");

        if (!Number.isInteger(request.body.max_frame_bytes) || request.body.max_frame_bytes < 256
                || !Number.isInteger(request.body.max_widgets) || request.body.max_widgets < 1
                || !Number.isInteger(request.body.max_queued_events) || request.body.max_queued_events < 1)
            return encodeError("invalid_limits", "negotiated renderer limits are invalid");

        const nextSession = request.session_id;
        const nextGeneration = request.generation;
        if (typeof nextSession !== "string" || nextSession.length === 0
                || !Number.isInteger(nextGeneration) || nextGeneration < 1)
            return encodeError("invalid_session", "negotiation requires a session id and generation");

        if (sessionId !== nextSession || generation !== nextGeneration) {
            widgets = ({});
            widgetModel.clear();
            eventQueue = [];
            nextEventCursor = 1;
            eventOverflow = false;
            revision = 0;
        }
        sessionId = nextSession;
        generation = nextGeneration;
        negotiatedVersion = selected;
        maxFrameBytes = Math.min(maxFrameBytes, request.body.max_frame_bytes);
        maxWidgets = Math.min(maxWidgets, request.body.max_widgets);
        maxQueuedEvents = Math.min(maxQueuedEvents, request.body.max_queued_events);

        return JSON.stringify({
            protocol: "execute.renderer",
            version: selected,
            request_id: request.request_id,
            session_id: sessionId,
            generation: generation,
            revision: request.revision,
            body: {
                kind: "negotiated",
                renderer: "quickshell-qml",
                supported: {minimum: protocolMinimum, maximum: protocolMaximum},
                max_frame_bytes: maxFrameBytes,
                max_widgets: maxWidgets,
                max_queued_events: maxQueuedEvents
            }
        });
    }

    function instantiate(request): string {
        const body = request.body;
        const key = widgetKey(body.applet_id, body.widget_id);
        if (!validIdentity(body.applet_id) || !validIdentity(body.widget_id))
            return encodeError("invalid_widget_identity", "applet and widget ids must be simple identifiers");
        if (widgets[key] !== undefined)
            return encodeError("duplicate_widget", "widget is already active");
        if (Object.keys(widgets).length >= maxWidgets)
            return encodeError("widget_limit", "renderer widget limit has been reached");
        if (!body.contract || !validComponentContract(body.contract))
            return encodeError("invalid_widget_contract", "component contract is unsupported");
        if (!validateRecord(body.contract.properties.schema, body.properties, true))
            return encodeError("invalid_properties", "initial properties do not match the declared schema");
        if (request.revision <= revision)
            return encodeError("stale_revision", "renderer revision must increase");

        const props = copyObject(body.properties);
        widgets[key] = {
            appletId: body.applet_id,
            widgetId: body.widget_id,
            component: body.contract.component,
            contract: body.contract,
            properties: props,
            revision: request.revision
        };
        widgetModel.append({
            widgetKey: key,
            appletId: body.applet_id,
            widgetId: body.widget_id,
            componentName: body.contract.component,
            text: props.text,
            enabled: body.contract.component === "button" ? props.enabled : true,
            sourceRevision: request.revision
        });
        revision = request.revision;
        return applied(request);
    }

    function patch(request): string {
        const body = request.body;
        const key = widgetKey(body.applet_id, body.widget_id);
        const widget = widgets[key];
        if (widget === undefined)
            return encodeError("unknown_widget", "cannot patch a widget that is not active");
        if (!validateRecord(widget.contract.properties.schema, body.properties, false))
            return encodeError("invalid_properties", "property patch does not match the declared schema");
        if (request.revision <= revision)
            return encodeError("stale_revision", "renderer revision must increase");

        for (const name in body.properties)
            widget.properties[name] = body.properties[name];
        widget.revision = request.revision;
        const index = findModelIndex(key);
        if (index < 0)
            return encodeError("missing_model_item", "widget model item disappeared");
        if (body.properties.text !== undefined)
            widgetModel.setProperty(index, "text", body.properties.text);
        if (body.properties.enabled !== undefined)
            widgetModel.setProperty(index, "enabled", body.properties.enabled);
        widgetModel.setProperty(index, "sourceRevision", request.revision);
        revision = request.revision;
        return applied(request);
    }

    function destroy(request): string {
        const body = request.body;
        const key = widgetKey(body.applet_id, body.widget_id);
        if (widgets[key] === undefined)
            return encodeError("unknown_widget", "cannot destroy a widget that is not active");
        if (request.revision <= revision)
            return encodeError("stale_revision", "renderer revision must increase");
        const index = findModelIndex(key);
        if (index >= 0)
            widgetModel.remove(index);
        delete widgets[key];
        revision = request.revision;
        return applied(request);
    }

    function pollEvents(request): string {
        const body = request.body;
        if (!Number.isInteger(body.after_cursor) || body.after_cursor < 0
                || !Number.isInteger(body.limit) || body.limit < 1 || body.limit > maxQueuedEvents)
            return encodeError("invalid_event_cursor", "event poll cursor or limit is invalid");
        const oldest = eventQueue.length > 0 ? eventQueue[0].cursor : nextEventCursor;
        if (eventOverflow || body.after_cursor + 1 < oldest)
            return JSON.stringify(eventBatch(request, [], Math.max(body.after_cursor, oldest - 1), oldest, true));
        const events = eventQueue.filter((event) => event.cursor > body.after_cursor).slice(0, body.limit);
        const next = events.length > 0 ? events[events.length - 1].cursor : body.after_cursor;
        return JSON.stringify(eventBatch(request, events, next, oldest, false));
    }

    function ackEvents(request): string {
        const through = request.body.through_cursor;
        if (!Number.isInteger(through) || through < 0 || through >= nextEventCursor)
            return encodeError("invalid_event_ack", "event acknowledgement is outside the emitted cursor range");
        eventQueue = eventQueue.filter((event) => event.cursor > through);
        return applied(request);
    }

    function queueSignal(key: string, appletId: string, widgetId: string, name: string, sourceRevision: int) {
        if (eventQueue.length >= maxQueuedEvents) {
            eventOverflow = true;
            return;
        }
        const widget = widgets[key];
        if (widget === undefined || widget.revision !== sourceRevision || generation < 1)
            return;
        eventQueue.push({
            cursor: nextEventCursor++,
            applet_id: appletId,
            widget_id: widgetId,
            signal: name,
            generation: generation,
            source_revision: sourceRevision,
            payload: {}
        });
    }

    function eventBatch(request, events, nextCursor: int, oldestCursor: int, overflow: bool) {
        return {
            protocol: "execute.renderer",
            version: negotiatedVersion,
            request_id: request.request_id,
            session_id: sessionId,
            generation: generation,
            revision: request.revision,
            body: {
                kind: "event_batch",
                events: events,
                next_cursor: nextCursor,
                oldest_cursor: oldestCursor,
                overflow: overflow
            }
        };
    }

    function applied(request): string {
        return JSON.stringify({
            protocol: "execute.renderer",
            version: negotiatedVersion,
            request_id: request.request_id,
            session_id: sessionId,
            generation: generation,
            revision: request.revision,
            body: {kind: "applied"}
        });
    }

    function encodeError(code: string, message: string): string {
        const request = activeRequest || ({});
        return JSON.stringify({
            protocol: "execute.renderer",
            version: request.version || 0,
            request_id: request.request_id || 0,
            session_id: request.session_id || sessionId,
            generation: Number.isInteger(request.generation) ? request.generation : generation,
            revision: Number.isInteger(request.revision) ? request.revision : revision,
            body: {kind: "error", code: code, message: message}
        });
    }

    function validComponentContract(contract): bool {
        if (contract.component !== "text" && contract.component !== "button")
            return false;
        const props = contract.properties;
        if (!props || props.delivery !== "single" || !props.schema || props.schema.type !== "record")
            return false;
        const required = props.schema.required || [];
        const signals = contract.signals || ({});
        if (contract.component === "text" && (!props.schema.fields || !props.schema.fields.text || props.schema.fields.text.type !== "text"
                    || props.schema.fields.text.nullable === true || !required.includes("text")
                    || Object.keys(props.schema.fields).some((key) => key !== "text")
                    || Object.keys(signals).length !== 0))
            return false;
        if (contract.component === "button") {
            if (!props.schema.fields || !props.schema.fields.text || props.schema.fields.text.type !== "text"
                    || props.schema.fields.text.nullable === true
                    || !props.schema.fields.enabled || props.schema.fields.enabled.type !== "boolean"
                    || props.schema.fields.enabled.nullable === true
                    || !required.includes("text") || !required.includes("enabled")
                    || Object.keys(props.schema.fields).some((key) => key !== "text" && key !== "enabled")
                    || Object.keys(signals).length !== 1 || signals.clicked === undefined)
                return false;
            const clicked = signals.clicked;
            if (!clicked || clicked.delivery !== "events" || !clicked.schema || clicked.schema.type !== "record"
                    || Object.keys(clicked.schema.fields || {}).length !== 0
                    || (clicked.schema.required || []).length !== 0)
                return false;
        }
        return true;
    }

    function validateRecord(schema, value, complete: bool): bool {
        if (!schema || schema.type !== "record" || value === null || Array.isArray(value)
                || typeof value !== "object")
            return false;
        const declared = schema.fields || ({});
        for (const key in value) {
            if (declared[key] === undefined || !matchesSchema(declared[key], value[key]))
                return false;
        }
        if (complete) {
            const required = schema.required || [];
            for (let index = 0; index < required.length; index++) {
                if (value[required[index]] === undefined)
                    return false;
            }
        }
        return true;
    }

    function matchesSchema(schema, value): bool {
        if (!schema)
            return false;
        switch (schema.type) {
        case "text":
        case "path":
            return typeof value === "string";
        case "integer":
            return typeof value === "number" && Number.isInteger(value);
        case "boolean":
            return typeof value === "boolean";
        case "record":
            return validateRecord(schema, value, true);
        case "list":
            if (!Array.isArray(value))
                return false;
            return value.every((item) => matchesSchema(schema.items, item));
        case "json":
            return true;
        default:
            return false;
        }
    }

    function copyObject(value) {
        return JSON.parse(JSON.stringify(value));
    }

    function findModelIndex(key: string): int {
        for (let index = 0; index < widgetModel.count; index++) {
            if (widgetModel.get(index).widgetKey === key)
                return index;
        }
        return -1;
    }

    function widgetKey(appletId: string, widgetId: string): string {
        return appletId + "::" + widgetId;
    }

    function validIdentity(value): bool {
        return typeof value === "string" && value.length > 0 && value.length <= 128
                && /^[A-Za-z0-9._-]+$/.test(value);
    }
}
